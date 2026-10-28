# CSCOM-compatible image store

This fork accepts existing CSCOM uploads without rebuilding the client library.
The extension is disabled unless `DUFS_CSCOM_UPLOAD_SK` is set before startup.

## Run

Use the same storage directory and public address/port as the old image store:

```sh
export DUFS_CSCOM_UPLOAD_SK='4f455a3d14caa5aa9cc3c037d49f1ef8c91c694f1134eb2562329634c877ce2f'
export DUFS_CSCOM_MAX_UPLOADS=16
dufs /data/images -b 0.0.0.0 -p 9800
```

The key above matches existing CSCOM clients. Put runtime environment settings
in your service configuration (for example a systemd EnvironmentFile); they are
not GitHub Actions secrets. Changing the key requires matching client changes.

Do not add `-A` or `--allow-upload` for this setup. The compatibility endpoint
has its own key check and does not need those flags. Leaving them off keeps
ordinary anonymous PUT/PATCH/DELETE operations disabled. Downloads remain public
by default. Normal dufs auth rules still govern normal file access; the CSCOM
key independently authorizes only the compatibility POST endpoint.

If the public address changes, set `CSCOM_IMAGE_STORE_URL` in the client process
before CSCOM operations. This is the existing client setting, not a new API.
Keeping the old public URL and storage directory preserves existing image links.

## Compatibility and performance

- `POST /upload/<filename>`: raw file body, `X-CSCOM-Upload-SK` header.
- `POST /upload?filename=<filename>` is also supported.
- `GET /<filename>`, HEAD and Range use normal dufs file serving.
- Successful uploads and duplicate regular files return HTTP 200. Duplicates
  retain the existing contents, including concurrent uploads of the same name.
- Flat filenames only; traversal, multipart bodies and staging names are rejected.
- Maximum body size: 20 MiB, checked for both declared and streamed lengths.
- Maximum active uploads: 16 by default; `DUFS_CSCOM_MAX_UPLOADS` accepts 1..4096.
  Excess requests receive HTTP 503 immediately instead of consuming an unbounded
  queue. Select the limit based on the server's bandwidth, storage and workload.
- Each upload streams through a 256 KiB disk buffer, rather than retaining the
  complete image. Uploads do not share a global write or publish mutex.
- Idle body timeout: 30 seconds; total upload timeout: 120 seconds.
- Temporary files reside in the storage directory. A same-filesystem hard link
  publishes completed data atomically without overwriting another upload. The
  storage filesystem must support hard links (for example ext4, XFS or NTFS).
- Staging files are excluded from listings, searches and archives, and direct
  requests to their reserved names are denied while compatibility mode is active.
- Temporary files are removed on ordinary completion, disconnect and timeout.
  Abrupt process termination can leave hidden `.cscom-upload-*.tmp` files. Remove
  these only while the server is stopped, with no other writer using the directory.
- In compatibility mode, `/ping` checks storage write/flush availability and
  returns 200 or 503. Unlike the old server it does not enforce a 100 MiB free
  space threshold; monitor disk capacity separately. The original
  `/__dufs__/health` endpoint remains available.

These changes have not been benchmarked. A larger concurrency limit alone does
not establish higher throughput.

## GitHub Actions

Enable workflows on the fork's Actions page. No custom secrets are required for
the following workflows:

- Push to `main`, pull request or manual CI dispatch: existing test/lint checks
  plus independent release builds downloadable as the `dufs-linux-x86_64` and
  `dufs-windows-x86_64` workflow artifacts. The Windows artifact contains
  `dufs.exe` for x86_64 Windows. The Linux artifact targets Linux x86_64 with
  the GitHub Ubuntu runner's system libraries; it is not a portable musl build.
- Push a version tag matching `v1.2.3` or `v1.2.3-cscom.1`: existing multi-platform
  release builds, including Linux x86_64/aarch64 musl binaries, uploaded to the
  fork's GitHub Releases using the automatically supplied `GITHUB_TOKEN`.

Example after committing and pushing your changes:

```sh
git tag v0.47.0-cscom.1
git push origin v0.47.0-cscom.1
```

Docker Hub and crates.io publishing jobs have been removed so they do not require
upstream publishing credentials. Repository or organization policy must allow
the Release job's `contents: write` permission. These workflows build server
executables, not Android client shared libraries.
