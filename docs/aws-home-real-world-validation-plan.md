# AWS-to-Home Real-World Validation Plan

**Status:** Ready for implementation
**Created:** 20 September 2026
**Purpose:** Final validation gate before extracting `journey-websocket`
**Deployment:** Disposable AWS EC2 gateway and containerized home agent

## 1. Goal

Run one realistic, reproducible deployment before moving
`crates/journey-websocket` into its own repository. The validation must exercise
the complete network and application path:

```text
Browser HTTPS/HTTP2
  -> Nginx container on disposable AWS EC2
  -> Rust gateway container
  -> Basic-authenticated WSS
  -> inner HTTP/2
  -> home container
  -> read-only media fixtures and writable upload storage
```

The test is intentionally larger than a ping/pong smoke test. It must prove
that the transport carries real image and video responses, byte ranges, a
large upload, concurrent streams, cancellation, and reconnection while
remaining within the memory budget of a micro instance.

Do not extract the crate until every required functional and resource check in
this document passes and the result has been recorded.

## 2. Scope

Implement only the application behavior needed to validate the transport:

- A static test page served through the AWS gateway.
- One preinstalled JPEG stored on home.
- One preinstalled, web-optimized MP4 stored on home.
- Complete and single-range media downloads.
- A raw streaming file upload from the browser to home.
- Separate Basic credentials for the site and home connection.
- TLS termination and public HTTP/2 through Nginx.
- The existing inner HTTP/2-over-WebSocket transport.
- Repeatable command-line checks and a short browser checklist.
- Memory, cancellation, outage, and reconnection measurements.

The validation does not add the production Journey website, database, cache,
post model, media transformations, adaptive streaming, CI, hosted monitoring,
or final public-key authentication.

The test website and its object routes are validation fixtures. They are not
the final Journey application API and must not be moved into the extracted
transport crate.

## 3. Fixed deployment decisions

- AWS host: disposable Ubuntu x86 `t3.micro`.
- AWS root disk: 16 GiB gp3.
- Public address: Elastic IP with a user-controlled test-domain `A` record.
- Packaging: Docker Compose on AWS and home.
- Reverse proxy: Nginx in the AWS Compose project.
- TLS: private, self-signed test CA with a domain certificate.
- Authentication: Basic Authentication implemented by the Rust gateway.
- Credentials: separate site and home credentials.
- Maximum upload: 256 MiB.
- Fixtures: bind-mounted read-only from the home host.
- Uploads: separate writable bind mount on the home host.
- Test driver: automated curl script plus manual browser checks.
- Image build: local x86 build, not compilation on the micro instance.
- Image delivery: direct Docker image stream over SSH; no registry.

## 4. Public interfaces

### 4.1 Browser-facing HTTPS routes

The gateway exposes these routes behind Nginx:

| Method | Path | Behavior |
|---|---|---|
| `GET` | `/` | Return the embedded static validation page. |
| `GET`, `HEAD` | `/media/image.jpg` | Proxy the preinstalled home JPEG. |
| `GET`, `HEAD` | `/media/video.mp4` | Proxy the preinstalled home MP4, including one byte range. |
| `POST` | `/api/uploads` | Stream a raw request body to home, limited to 256 MiB. |
| `GET` | `/health` | Verify the gateway can complete an inner request to home. |
| WebSocket upgrade | `/internal/storage` | Establish the authenticated home storage session. |

All ordinary public routes require the site credential. The WebSocket route
requires the separate home credential.

An unauthenticated container-only liveness route may be added for Compose, but
Nginx must not publish it.

### 4.2 Static validation page

The page is static HTML, CSS, and JavaScript embedded in or packaged with the
gateway. It requires no Node.js runtime.

It must contain:

- An `<img>` using `/media/image.jpg`.
- A native `<video controls>` element using `/media/video.mp4`.
- A file input and upload button.
- JavaScript that sends the selected `File` directly as the request body to
  `/api/uploads` rather than constructing multipart form data.
- A status area showing success or failure, stored byte count, and SHA-256.

The page must not expose a route for downloading an uploaded file.

### 4.3 Inner HTTP/2 operations

The gateway maps public operations onto these inner requests:

| Method | Inner path | Behavior |
|---|---|---|
| `GET`, `HEAD` | `/fixtures/image.jpg` | Read the configured JPEG fixture. |
| `GET`, `HEAD` | `/fixtures/video.mp4` | Read the configured MP4 fixture. |
| `PUT` | `/uploads` | Persist a streaming upload by its SHA-256 digest. |
| `GET` | `/health` | Return a small fixed healthy response. |

The transport crate must remain unaware of these paths and operations.

## 5. Authentication and TLS

### 5.1 Credential separation

Use four environment variables:

```text
JOURNEY_SITE_USERNAME
JOURNEY_SITE_PASSWORD
JOURNEY_HOME_USERNAME
JOURNEY_HOME_PASSWORD
```

The site credential authorizes `/`, `/media/*`, `/api/uploads`, and the public
health route. The home credential authorizes only `/internal/storage`.

Required behavior:

- Missing or invalid credentials return `401 Unauthorized`.
- Rejections include an appropriate `WWW-Authenticate` header.
- Compare credentials in constant time.
- Never log an `Authorization` header, username, or password.
- The site credential must not open or replace a home session.
- The home credential must not access the page, media, health, or upload API.
- Store credentials only in uncommitted environment files with restrictive
  filesystem permissions.

Basic credentials are acceptable for this validation because they are sent
only through verified TLS. They are not the final production authentication
design.

### 5.2 Transport-crate authorization hook

The existing crate owns the WebSocket server handshake, including limits and
subprotocol negotiation. Add a generic server-side authorization hook so the
gateway can authenticate the HTTP upgrade without duplicating the crate's
handshake logic.

The new API should have behavior equivalent to:

```rust
accept_websocket_with_authorizer(stream, config, authorize).await
```

The synchronous `authorize` callback receives the handshake request and
returns either success or a complete HTTP rejection response. The crate must
still:

- Validate `Config` before reading the handshake.
- Install its bounded `WebSocketConfig`.
- Require and select `h2-over-websocket-v1`.
- Reject an invalid subprotocol even when authorization succeeds.
- Start no inner HTTP/2 work when authorization rejects the request.

The current `accept_websocket` remains as a convenience API and delegates to
the new entry point with an allow-all authorizer. Basic Authentication remains
gateway policy and must not appear in the reusable crate.

Add focused tests for successful authorization, rejection before HTTP/2,
subprotocol enforcement after successful authorization, and preservation of
the configured WebSocket limits.

### 5.3 WSS client support

Enable the `tokio-tungstenite` Rustls native-root feature required by
`wss://`. The home application must construct a WebSocket handshake request
containing its Basic authorization header and pass that request to the existing
client connection API.

Certificate and hostname verification must remain enabled. Do not provide an
insecure connector or a verification-bypass option.

### 5.4 Private test CA

Create a private test CA on the development machine and use it to sign a
server certificate whose subject alternative name exactly matches the test
domain.

- Mount the leaf certificate and private key into Nginx.
- Import the CA certificate into the browser or operating-system trust store.
- Mount only the CA certificate into the home container.
- Configure the home container so native-root loading includes that CA.
- Never use `curl -k`, disable hostname checks, or accept arbitrary
  certificates.
- Never copy the CA private key to AWS or home.
- Keep certificates, keys, and environment files out of Git.

## 6. Streaming behavior

### 6.1 Media downloads

Home opens the configured fixture only after validating the exact inner path.
It reads bounded chunks and sends a chunk only after the HTTP/2 stream grants
capacity. It must not read the complete file or build an unbounded queue.

The gateway forwards response status and these headers where applicable:

- `Content-Type`
- `Content-Length`
- `Accept-Ranges`
- `Content-Range`
- `ETag`

Gateway response flow control is released only as Nginx and the public client
consume the body.

### 6.2 Range requests

Support exactly one `bytes` range. Required forms are:

```text
bytes=0-1023
bytes=1048576-
bytes=-65536
```

Return:

- `200 OK` for a complete response.
- `206 Partial Content` with an exact `Content-Range` for a valid range.
- `416 Range Not Satisfiable` with `Content-Range: bytes */<length>` for an
  unsatisfiable range.
- A bounded client error for malformed or multiple ranges.

`HEAD` returns the same metadata and status selection as `GET` but no DATA
frames.

### 6.3 Uploads

The browser sends the selected file as the raw request body. The gateway:

- Rejects `Content-Length` above 256 MiB before starting the inner request.
- Counts observed bytes and aborts if a chunked body crosses the limit.
- Polls the public request body only when the inner HTTP/2 request stream can
  accept more DATA.
- Resets or drops the inner stream when the browser disconnects.
- Waits no more than ten seconds for inner response headers.

Home:

- Independently enforces the 256 MiB limit.
- Creates a temporary file inside the upload filesystem.
- Writes each received chunk before releasing its HTTP/2 receive capacity.
- Calculates SHA-256 incrementally.
- Flushes and synchronizes the completed file.
- Atomically renames it to the lowercase hexadecimal digest.
- Treats an already-present digest as a successful duplicate.
- Deletes its temporary file after errors, oversize bodies, or cancellation.
- Returns JSON containing `sha256` and `size`.

Neither gateway nor home may retain the full upload in memory.

## 7. Container deployment

### 7.1 Image build and delivery

Build the AMD64 release image on the x86 home/development host. Do not compile
Rust on the 1 GiB EC2 instance.

Tag the image with the tested Git commit:

```text
journey-real-test:<git-sha>
```

Create a Docker SSH context:

```bash
docker context create journey-aws --docker host=ssh://ubuntu@<aws-host>
```

Stream the locally built image directly into the remote Docker daemon:

```bash
docker image save journey-real-test:<git-sha> \
  | docker --context journey-aws image load
```

This avoids a temporary archive and a separate SCP/load sequence. It does not
provide registry-style delta transfer: the save stream still contains the
selected image's layers, although the remote daemon deduplicates layers while
loading them. Do not describe this as pushing only missing layers.

Use the remote context for operational inspection:

```bash
docker --context journey-aws ps
docker --context journey-aws logs <container>
docker --context journey-aws stats
```

Copy the small AWS deployment bundle to `/opt/journey-test` over SSH. Start
Compose through SSH from that directory so environment files, bind mounts,
certificates, and absolute paths resolve on EC2.

### 7.2 AWS Compose project

Run two containers on a private bridge network:

#### Nginx

- Publish host port 443 only.
- Hard memory limit: 64 MiB.
- Terminate TLS and advertise HTTP/2.
- Proxy ordinary HTTP to gateway port 8080.
- Proxy `/internal/storage` to gateway port 9000 using WebSocket upgrade
  headers.
- Preserve `Authorization`.
- Set `client_max_body_size 256m`.
- Disable request buffering for `/api/uploads`.
- Disable response buffering for media and WebSocket routes.
- Use bounded but streaming-compatible timeouts.
- Keep logs bounded through Docker logging options.

#### Gateway

- Publish no host ports.
- Hard memory limit: 192 MiB.
- Bind HTTP and WebSocket listeners only inside the Compose network.
- Store no media or upload bodies on AWS.
- Use a read-only root filesystem and temporary `tmpfs` paths where needed.
- Drop unnecessary capabilities and set `no-new-privileges`.

### 7.3 Home Compose project

Run the application image using the home command:

- Publish no ports.
- Hard memory limit: 256 MiB.
- Connect to `wss://<test-domain>/internal/storage`.
- Load the home credential from an uncommitted environment file.
- Mount the private CA certificate read-only.
- Mount fixture media read-only.
- Mount upload storage writable.
- Run as the existing non-root application user.
- Drop unnecessary capabilities and set `no-new-privileges`.
- Restart unless explicitly stopped for an outage test.

The JPEG must be a valid image. The MP4 must be browser-playable,
web-optimized for fast start, and at least 50 MiB so throttling and seeking are
meaningful.

## 8. AWS setup and teardown

Create through the AWS console:

- Ubuntu x86 `t3.micro`.
- 16 GiB gp3 root volume.
- Elastic IP.
- Test-domain `A` record pointing at the Elastic IP.
- Security group allowing TCP 443 publicly.
- Security group allowing TCP 22 only from the administrator's current public
  IP.
- No rules for public ports 80, 8080, or 9000.

Install Docker Engine and the Compose plugin, add the deployment user to the
Docker group, load the application image, copy the deployment bundle, and
start the AWS Compose project.

After testing:

1. Stop and remove the Compose project.
2. Terminate the EC2 instance.
3. Release the Elastic IP.
4. Remove the DNS record.
5. Revoke both Basic credentials.
6. Delete the server private key and test deployment bundle.
7. Remove the Docker SSH context.

## 9. Automated validation script

Add a curl-based script that reads its URL, site credential, expected fixture
sizes and hashes, and AWS SSH target from environment or protected temporary
configuration. Do not place passwords directly in command arguments that will
be recorded in shell history. Use a permission-restricted temporary curl
configuration or netrc file and delete it on exit.

The script must:

1. Confirm missing and incorrect credentials return `401`.
2. Confirm the home credential cannot access site routes.
3. Confirm the site credential cannot establish the home WebSocket.
4. Verify `/health` completes through the established home session.
5. Download the JPEG and MP4 and compare their SHA-256 digests.
6. Verify `HEAD` status, length, type, and range metadata.
7. Verify beginning, middle, ending, open-ended, and suffix ranges
   byte-for-byte against the home fixtures.
8. Verify malformed, multiple, and unsatisfiable ranges return bounded errors.
9. Generate and upload a deterministic 256 MiB file.
10. Compare its local digest, response digest, reported size, and stored home
    file.
11. Start a throttled MP4 download and request the JPEG concurrently.
12. Require the JPEG to complete within five seconds.
13. Cancel the throttled download.
14. Require health and a fresh media request to succeed within five seconds on
    the same home session.
15. Repeat large transfers and confirm no temporary upload files remain.

The test output must use a temporary results directory and must not record
credentials.

## 10. Manual lifecycle and browser checks

### Browser

- Open the domain without a TLS warning after installing the test CA.
- Enter the site credential when challenged.
- Confirm the JPEG renders.
- Confirm the MP4 begins playback.
- Seek to the beginning, middle, and near the end.
- Upload a file and confirm the displayed size and digest.
- Cancel an upload or navigate away, then confirm later operations still work.

### Home outage

1. Start active media traffic.
2. Stop the home container.
3. Require affected requests to fail within ten seconds rather than hang.
4. Restart home.
5. Require `/health` to recover within fifteen seconds.

### Gateway restart

1. Leave home running.
2. Restart the gateway container.
3. Require home to reconnect without manual restart.
4. Require `/health` to recover within fifteen seconds.

### Nginx restart

1. Restart Nginx, breaking the WSS connection.
2. Require home to reconnect once TLS service returns.
3. Confirm media and uploads still work.

## 11. Resource acceptance

Record `docker stats`, container restart counts, and relevant system memory at
these points:

- Idle after connection establishment.
- Complete video download.
- Range download.
- 256 MiB upload.
- Throttled video plus concurrent image.
- Cancelled transfer.
- Home outage and reconnection.
- Gateway and Nginx restarts.
- After repeated large transfers.

The deployment passes only if:

- Gateway peak RSS remains below 160 MiB.
- Nginx peak RSS remains below 48 MiB.
- Home remains within its 256 MiB hard limit.
- No container is OOM-killed or unexpectedly restarted.
- Gateway RSS returns to within 20 MiB of its idle baseline after testing.
- AWS disk use does not grow with proxied media, excluding bounded logs and
  normal container metadata.
- A slow client causes backpressure rather than memory growth proportional to
  the object size.

## 12. Result record and extraction gate

Create a dated result document after the real test. Record:

- Git commit and application image tag.
- Crate dependency versions.
- EC2 instance type, AMI, and Docker versions.
- Home host architecture and Docker versions.
- Domain used, without credentials.
- Fixture names, sizes, and SHA-256 digests.
- Upload size and SHA-256 digest.
- Automated and browser test results.
- Cancellation and reconnection timings.
- Idle and peak resource measurements.
- Container restart and OOM status.
- Sanitized relevant logs.
- Every deviation, failure, and retest.
- Final pass or fail decision.

The crate may move to its own repository only after:

- Every functional check passes.
- Every resource bound passes.
- Authentication and TLS rejection checks pass.
- No unresolved transport defect remains.
- The result document is complete.

Failure does not automatically invalidate the architecture. Record the exact
failure, fix it in the current repository, repeat the affected tests, and then
repeat the complete final sequence before extraction.

## 13. Repository constraints

- Do not run `cargo fmt`, `cargo fmt --check`, `rustfmt`, or another automated
  source formatter.
- Preserve the existing formatting with manual, targeted edits.
- Never commit passwords, private keys, generated certificates, fixture media,
  uploaded data, Docker exports, or validation output.
- Keep application routes and storage behavior outside `journey-websocket`.
- Keep authentication policy outside `journey-websocket`; only the generic
  handshake authorization hook belongs there.
- Treat the AWS instance and credentials as disposable test resources.
