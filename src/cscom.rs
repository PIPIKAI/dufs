//! Opt-in wire compatibility for existing CSCOM clients.
use super::{body_full, fs, IncomingStream, Request, Response, Server};
use anyhow::{Context, Result};
use futures_util::TryStreamExt;
use hyper::{header::CONTENT_LENGTH, StatusCode};
use std::{path::PathBuf, time::Duration};
use tokio::{io::AsyncWriteExt, sync::Semaphore, time::timeout};
use uuid::Uuid;

const MAX_BYTES: u64 = 20 * 1024 * 1024;
const STAGING_PREFIX: &str = ".cscom-upload-";

pub(super) struct CscomUpload {
    key: Vec<u8>,
    slots: Semaphore,
}

impl CscomUpload {
    pub(super) fn from_env() -> Result<Option<Self>> {
        let Some(key) = std::env::var_os("DUFS_CSCOM_UPLOAD_SK") else {
            return Ok(None);
        };
        let key = key
            .into_string()
            .map_err(|_| anyhow::anyhow!("Invalid CSCOM key"))?;
        anyhow::ensure!(!key.is_empty(), "DUFS_CSCOM_UPLOAD_SK must not be empty");
        let limit = match std::env::var("DUFS_CSCOM_MAX_UPLOADS") {
            Ok(value) => value
                .parse::<usize>()
                .context("Invalid DUFS_CSCOM_MAX_UPLOADS")?,
            Err(std::env::VarError::NotPresent) => 16,
            Err(error) => return Err(error.into()),
        };
        anyhow::ensure!(
            (1..=4096).contains(&limit),
            "DUFS_CSCOM_MAX_UPLOADS must be 1..4096"
        );
        Ok(Some(Self {
            key: key.into_bytes(),
            slots: Semaphore::new(limit),
        }))
    }

    fn authorized(&self, value: &[u8]) -> bool {
        // Compare all bytes of equal-length keys without an early mismatch exit.
        value.len() == self.key.len()
            && value
                .iter()
                .zip(&self.key)
                .fold(0u8, |diff, (a, b)| diff | (a ^ b))
                == 0
    }
}

// Covers disconnects, timeouts and cancellation as well as normal completion.
// The file handle is declared after this guard and is closed before unlinking.
struct TemporaryUpload(PathBuf);
impl Drop for TemporaryUpload {
    fn drop(&mut self) {
        let path = self.0.clone();
        // Unlink through Tokio's filesystem pool rather than blocking a network
        // worker on metadata I/O. Reserved names remain inaccessible until gone.
        tokio::spawn(async move {
            if let Err(error) = fs::remove_file(path).await {
                if error.kind() != std::io::ErrorKind::NotFound {
                    log::warn!("CSCOM temporary upload cleanup failed: {error}");
                }
            }
        });
    }
}

fn reply(status: StatusCode, message: &'static str) -> Response {
    let mut res = Response::new(body_full(message));
    *res.status_mut() = status;
    res
}

impl Server {
    pub(super) async fn handle_cscom_health(&self, head_only: bool) -> Result<Response> {
        let temporary = TemporaryUpload(
            self.args
                .serve_path
                .join(format!("{STAGING_PREFIX}{}.tmp", Uuid::new_v4())),
        );
        let probe = async {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary.0)
                .await?;
            file.write_all(b"ok").await?;
            file.flush().await
        };
        let status = match timeout(Duration::from_secs(5), probe).await {
            Ok(Ok(())) => StatusCode::OK,
            _ => StatusCode::SERVICE_UNAVAILABLE,
        };
        Ok(reply(
            status,
            if head_only {
                ""
            } else if status.is_success() {
                "OK"
            } else {
                "Storage unavailable"
            },
        ))
    }

    pub(super) async fn handle_cscom_upload(
        &self,
        req: Request,
        relative_path: &str,
        config: &CscomUpload,
    ) -> Result<Response> {
        let authorized = req
            .headers()
            .get("X-CSCOM-Upload-SK")
            .is_some_and(|value| config.authorized(value.as_bytes()));
        if !authorized {
            return Ok(reply(
                StatusCode::UNAUTHORIZED,
                "Upload authorization failed",
            ));
        }
        // The legacy header only authorizes this route, never normal WebDAV writes.
        let filename = if relative_path == "upload" {
            form_urlencoded::parse(req.uri().query().unwrap_or_default().as_bytes())
                .find(|(key, _)| key == "filename")
                .map(|(_, value)| value.into_owned())
                .unwrap_or_else(|| "unnamed.txt".to_owned())
        } else {
            relative_path
                .strip_prefix("upload/")
                .unwrap_or_default()
                .to_owned()
        };
        // Flat object names only, including on Windows; reserve staging names.
        if filename.is_empty()
            || filename.starts_with('.')
            || filename.ends_with('.')
            || filename.ends_with(' ')
            || filename
                .chars()
                .any(|c| c.is_control() || "/\\:<>\"|?*".contains(c))
        {
            return Ok(reply(StatusCode::BAD_REQUEST, "Unsafe upload filename"));
        }
        if req.headers().get("content-type").is_some_and(|v| {
            v.to_str()
                .unwrap_or_default()
                .to_ascii_lowercase()
                .starts_with("multipart/")
        }) {
            return Ok(reply(
                StatusCode::BAD_REQUEST,
                "Multipart upload is not supported",
            ));
        }
        let expected = match req.headers().get(CONTENT_LENGTH) {
            Some(value) => match value.to_str().ok().and_then(|v| v.parse::<u64>().ok()) {
                Some(value) => Some(value),
                None => return Ok(reply(StatusCode::BAD_REQUEST, "Invalid Content-Length")),
            },
            None => None,
        };
        if expected.is_some_and(|size| size > MAX_BYTES) {
            return Ok(reply(
                StatusCode::PAYLOAD_TOO_LARGE,
                "Upload exceeds size limit",
            ));
        }
        let Ok(_permit) = config.slots.try_acquire() else {
            return Ok(reply(
                StatusCode::SERVICE_UNAVAILABLE,
                "Too many active uploads",
            ));
        };
        let target = self.args.serve_path.join(&filename);
        let temporary = TemporaryUpload(
            self.args
                .serve_path
                .join(format!("{STAGING_PREFIX}{}.tmp", Uuid::new_v4())),
        );
        let operation = async {
            let file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary.0)
                .await?;
            // Coalesce network frames into bounded 256 KiB disk writes per upload.
            let mut file = tokio::io::BufWriter::with_capacity(256 * 1024, file);
            let mut stream = IncomingStream::new(req.into_body());
            let mut received = 0u64;
            loop {
                let chunk = match timeout(Duration::from_secs(30), stream.try_next()).await {
                    Ok(Ok(chunk)) => chunk,
                    Ok(Err(_)) => return Ok(reply(StatusCode::BAD_REQUEST, "Incomplete upload")),
                    Err(_) => return Ok(reply(StatusCode::REQUEST_TIMEOUT, "Upload idle timeout")),
                };
                let Some(chunk) = chunk else { break };
                received += chunk.len() as u64;
                if received > MAX_BYTES {
                    return Ok(reply(
                        StatusCode::PAYLOAD_TOO_LARGE,
                        "Upload exceeds size limit",
                    ));
                }
                file.write_all(&chunk).await?;
            }
            if expected.is_some_and(|size| size != received) {
                return Ok(reply(StatusCode::BAD_REQUEST, "Incomplete upload"));
            }
            file.flush().await?;
            drop(file);
            // Same-filesystem link atomically publishes complete content without
            // replacing an existing file, even with concurrent duplicate uploads.
            match fs::hard_link(&temporary.0, &target).await {
                Ok(()) => Ok(reply(StatusCode::OK, "OK")),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let metadata = fs::symlink_metadata(&target).await?;
                    if metadata.file_type().is_file() {
                        Ok(reply(StatusCode::OK, "Already exists"))
                    } else {
                        Ok(reply(StatusCode::CONFLICT, "Target is not a regular file"))
                    }
                }
                Err(error) => Err(error.into()),
            }
        };
        match timeout(Duration::from_secs(120), operation).await {
            Ok(result) => result,
            Err(_) => Ok(reply(
                StatusCode::REQUEST_TIMEOUT,
                "Upload duration exceeded",
            )),
        }
    }
}
