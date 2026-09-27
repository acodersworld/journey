# Vertical Slice 01 Checkpoint 1: Historical Implementation Plan

**Status:** Historical; superseded by the current transport and storage crates

**Updated:** 19 September 2026  
**Implements:** [Vertical Slice 01: HTTP/2 over WebSocket Object Download](../vertical-slice-01-http2-over-websocket.md)

**This plan covers:** Checkpoint 1 only — HTTP/2 over a transferable Tokio byte-stream boundary

**Extraction review:** [HTTP/2-over-WebSocket Crate Extraction Review](../../deferred/h2-over-websocket-crate-extraction.md)

This document preserves the original checkpoint plan for historical context.
Its proposed layout, dependencies, verification commands, and future-tense
steps are not current instructions. Follow the repository's `AGENTS.md` and
the current design documents for ongoing work.

## 1. Goal

The immediate goal is to determine whether HTTP/2 is a good protocol to carry inside the already-selected WebSocket transport.

Checkpoint 1 will implement the object-transfer protocol over `tokio::io::duplex`. Tokio duplex is only a controlled test transport. It is not intended to become part of the deployed network topology.

The implementation must establish a transport boundary that can later be supplied by a WebSocket adapter without changing:

- HTTP/2 client or server behavior.
- Object request paths, methods, headers, or statuses.
- File lookup and range handling.
- Streaming and flow-control logic.
- Cancellation behavior.
- Public object-download handlers.

Checkpoint 2 will replace the direct duplex pair with a local duplex stream connected to a WebSocket bridge on each machine. Only connection establishment, bridge lifecycle, and reconnect handling should change.

```text
Checkpoint 1

HTTP/2 client <────── tokio::io::duplex ──────> HTTP/2 server


Checkpoint 2

HTTP/2 client                              HTTP/2 server
      │                                          │
local duplex                                local duplex
      │                                          │
WS bridge ───────────── WebSocket ───────── WS bridge
```

## 2. Deliverable

Checkpoint 1 produces:

- A Rust workspace with reusable object-model and HTTP/2 crates.
- A runnable single-process proof of concept.
- A public HTTP interface usable with `curl` or a browser.
- An inner HTTP/2 client and server connected by a bounded Tokio duplex stream.
- Committed JPEG and MP4 fixtures in the content-addressed object layout.
- Automated unit, protocol, flow-control, cancellation, and public-interface tests.
- A single non-root Docker image for repeatable manual testing.
- Recorded observations used to decide whether HTTP/2 should proceed to the WebSocket checkpoint.

The following are explicitly deferred:

- WebSocket implementation.
- Separate gateway and home processes.
- Two-machine deployment.
- WSS, HTTP Basic, and Ed25519 authentication.
- Reconnection and heartbeat behavior.
- Uploads, deletion, and object listing.
- AWS caching and SQLite.
- React and the public website.
- CI and production monitoring.

## 3. Proposed workspace

Use a Rust 2024 workspace with these packages:

```text
journey/
├── Cargo.toml
├── rust-toolchain.toml
├── crates/
│   ├── journey-core/
│   │   └── src/
│   └── journey-h2/
│       └── src/
├── apps/
│   └── journey-duplex-poc/
│       └── src/
├── fixtures/
│   ├── manifest.json
│   └── object-store/
├── Dockerfile
└── docs/
```

Responsibilities:

| Package | Responsibility |
|---|---|
| `journey-core` | Object identifiers, metadata, range parsing, filesystem layout, and read-only object access |
| `journey-h2` | Transport-independent HTTP/2 object client, server, streaming, limits, and errors |
| `journey-duplex-poc` | Process startup, duplex connection creation, public Axum routes, health, and shutdown |

Use current compatible releases of Tokio, `h2`, Axum, `http`, `http-body`, `bytes`, Serde, SHA-2, `thiserror`, and `tracing`. Pin the Rust toolchain used by the project. Do not add a WebSocket dependency during this checkpoint.

## 4. Permanent transport boundary

### 4.1 Transport contract

Define a public marker trait in `journey-h2`:

```rust
pub trait InnerTransport:
    AsyncRead + AsyncWrite + Unpin + Send + 'static
{
}

impl<T> InnerTransport for T
where
    T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
}
```

The two HTTP/2 entry points accept only this abstraction:

```rust
pub async fn connect_object_client<T>(
    io: T,
    limits: H2Limits,
) -> Result<ObjectClient, H2Error>
where
    T: InnerTransport;

pub async fn serve_objects<T>(
    io: T,
    store: FsObjectStore,
    limits: H2Limits,
) -> Result<(), H2Error>
where
    T: InnerTransport;
```

The names may be adjusted to normal Rust module conventions, but the separation and generic boundary must remain.

### 4.2 Boundary rules

The HTTP/2 and object layers must not know:

- Whether the byte stream came from duplex, TCP, TLS, WebSocket, or another tunnel.
- Which peer initiated the outer connection.
- Whether the peers are in one process or on different machines.
- How WebSocket messages will be framed.
- How authentication, heartbeat, reconnection, or adapter shutdown works.

WebSocket types must never appear in:

- Object model APIs.
- Object storage APIs.
- HTTP/2 handlers.
- HTTP/2 client request methods.
- Response-body streaming types.
- Range or cancellation logic.

### 4.3 Checkpoint 1 connection provider

The proof-of-concept orchestration layer creates a bounded pair:

```rust
let (gateway_io, home_io) = tokio::io::duplex(256 * 1024);
```

It gives `gateway_io` to the inner HTTP/2 client and `home_io` to the inner HTTP/2 server. Nothing below the orchestration layer may depend on `DuplexStream` specifically.

### 4.4 Future WebSocket adapter contract

Checkpoint 2 will introduce a transport-specific module or crate with an interface equivalent to:

```rust
pub struct AdaptedTransport {
    pub io: DuplexStream,
    pub driver: JoinHandle<Result<(), TransportError>>,
}

pub fn adapt_websocket<W>(
    websocket: W,
    limits: BridgeLimits,
) -> AdaptedTransport;
```

The exact WebSocket generic type will depend on the selected library. The behavior is already fixed:

- `io` is the byte stream passed unchanged into the existing HTTP/2 entry point.
- `driver` owns the WebSocket and pumps bytes in both directions.
- Outgoing bytes become bounded binary WebSocket messages.
- Incoming binary-message payloads are concatenated into one ordered byte stream.
- WebSocket message boundaries are invisible to HTTP/2.
- Ping, pong, close, text messages, flush, shutdown, and protocol errors are handled by the driver.
- All queues and buffers are bounded.
- Adapter failure closes `io`, causing the existing HTTP/2 connection to fail normally.
- The orchestration layer observes driver termination and controls reconnect behavior.

This future adapter must plug into the same HTTP/2 functions. It must not require an HTTP/2 fork, wrapper protocol, or alternate request handlers.

## 5. Object model and filesystem store

Implement the following shared concepts:

```text
ObjectId
ObjectMetadata
RequestedRange
ResolvedRange
FsObjectStore
OpenedObject
```

### 5.1 Object identity

An object ID is exactly:

```text
sha256:<64 lowercase hexadecimal characters>
```

Validation must reject:

- Uppercase hexadecimal.
- Incorrect length.
- Missing or alternate prefixes.
- Separators, whitespace, percent-encoded path syntax, or traversal components.

### 5.2 Filesystem layout

Derive every path internally:

```text
<root>/<digest[0..2]>/<digest[2..4]>/<digest>.blob
<root>/<digest[0..2]>/<digest[2..4]>/<digest>.json
```

Requests never provide a filesystem path. The object root is read-only at runtime.

Sidecar metadata contains:

```json
{
  "object_id": "sha256:...",
  "size": 842193,
  "content_type": "image/jpeg"
}
```

Opening an object validates:

- The requested ID and sidecar ID match.
- The derived path is canonical and beneath the configured root.
- The object is a regular file.
- The declared and actual sizes match.
- The content type is one of the explicitly supported fixture types.
- Metadata size is bounded before it is read or parsed.

### 5.3 Range behavior

Support one byte range in each standard form:

```text
bytes=start-end
bytes=start-
bytes=-suffix-length
```

Resolve ranges only after the object size is known. Reject multiple ranges, malformed numbers, overflow, zero-length suffixes, and invalid ordering. A syntactically valid but unsatisfiable range returns `416 Range Not Satisfiable` and:

```http
Content-Range: bytes */<complete-size>
```

The resolved representation uses inclusive start and end offsets plus a checked length.

## 6. Inner HTTP/2 object protocol

### 6.1 Supported requests

The home-side server accepts only:

```text
GET  /objects/{object_id}
HEAD /objects/{object_id}
GET  /objects/{object_id} with one Range header
```

Require:

- HTTP version 2.
- Authority `home.internal`.
- A canonical path with no query string.
- An end-of-stream request with no body or trailers.
- Header sizes within the configured limit.

Reject all other methods and request shapes.

### 6.2 Responses

Complete responses include:

```http
200 OK
Content-Type: image/jpeg
Content-Length: <size>
Accept-Ranges: bytes
ETag: "sha256:<digest>"
```

Partial responses use `206 Partial Content` and add the exact `Content-Range`. `HEAD` returns the same metadata headers that `GET` would return, with no body.

Return bounded errors:

| Condition | Status |
|---|---:|
| Invalid object ID, malformed range, body, query, or authority | `400` |
| Missing well-formed object | `404` |
| Unsupported method | `405` |
| Unsatisfiable range | `416` |
| Unexpected storage failure | `500` |

Error bodies must be short constants and must not expose local paths or internal error chains. Do not implement server push or trailers.

### 6.3 Initial limits

Use explicit, centralized limits:

| Limit | Initial value |
|---|---:|
| Concurrent inner streams | 8 |
| Maximum header list | 16 KiB |
| Initial per-stream receive window | 64 KiB |
| Connection receive window | 512 KiB |
| File/DATA chunk | 16 KiB |
| Tokio duplex capacity | 256 KiB |

These values are prototype defaults, not permanent production tuning.

### 6.4 Server streaming

For each response stream:

1. Validate the complete request before opening a file.
2. Open and seek to the resolved starting offset.
3. Reserve no more than one 16 KiB HTTP/2 send-capacity request.
4. Wait for capacity before reading the corresponding number of file bytes.
5. Immediately send any bytes for which capacity was granted.
6. Repeat until the selected length is exhausted.
7. End the stream explicitly.

Do not feed unbounded data into `h2`; it can otherwise buffer data awaiting flow-control capacity. If the client resets the stream, stop reading immediately and release the file handle.

### 6.5 Client behavior

The inner client:

- Waits for `SendRequest::ready()` before creating each stream.
- Uses a short-held mutex only for readiness and stream creation.
- Uses a semaphore to cap active operations at eight.
- Builds version-2 requests with the fixed internal authority.
- Exposes response metadata and a streaming body to the public layer.
- Reports connection-driver termination through shared health state.

Inbound flow-control capacity is released only as response bytes advance toward the public consumer. Dropping a response body resets only its associated HTTP/2 stream.

## 7. Runnable proof of concept

### 7.1 Startup

The executable performs this sequence:

1. Read configuration.
2. Load and validate the committed fixture manifest.
3. Open the read-only object store.
4. Create the bounded Tokio duplex pair.
5. Start the inner HTTP/2 server with the home end.
6. Perform the client handshake with the gateway end.
7. Spawn and monitor the HTTP/2 connection driver.
8. Bind the public server only after the inner connection is ready.

### 7.2 Public interface

Expose:

```text
GET  /health
GET  /objects/{object_id}
HEAD /objects/{object_id}
GET  /objects/{object_id} with Range
```

`/health` returns bounded JSON indicating whether the inner HTTP/2 connection is ready. Object endpoints return `503 Service Unavailable` immediately when it is not ready.

The public handler validates object ID and range syntax before opening an inner stream. The home server still performs full independent validation.

### 7.3 Public response streaming

Implement an HTTP body around the inner `h2::RecvStream`:

- Yield DATA incrementally.
- Never collect a complete response.
- Release the preceding frame's receive capacity when the downstream consumer requests the next frame.
- Preserve the exact content length and range headers.
- Drop the inner response stream when the public body is dropped.

This ensures a slow or disconnected public client propagates bounded backpressure or cancellation to the inner stream.

### 7.4 Failure and shutdown

If either inner connection driver exits:

- Record the error through structured logging.
- Mark `/health` unavailable.
- Fail later object requests with `503`.
- Allow active streams to observe their normal HTTP/2 errors.

Automatic recreation of the duplex session is unnecessary. Reconnection belongs to the WebSocket checkpoint.

On process shutdown, stop accepting public requests, drop the client handles, allow the HTTP/2 tasks to terminate, and then exit.

### 7.5 Configuration

Use environment variables:

```text
JOURNEY_BIND=127.0.0.1:8080
JOURNEY_OBJECT_ROOT=fixtures/object-store
RUST_LOG=info
```

The container changes the bind address to `0.0.0.0:8080` and the object root to `/data/objects`.

## 8. Fixtures

Commit two generated, redistributable fixtures:

- A small JPEG test image.
- A valid browser-playable MP4 encoded as H.264 with `yuv420p` pixel format and fast-start metadata.

Store them directly in the content-addressed object layout. Add `fixtures/manifest.json` mapping stable friendly names to object IDs so tests and manual instructions do not hard-code IDs in source code.

Document the exact FFmpeg commands used to generate the fixtures. Add an integrity test that:

- Recalculates each SHA-256 digest.
- Confirms the directory and filename match the digest.
- Parses the sidecar.
- Confirms declared and actual sizes match.
- Confirms the expected MIME type.
- Confirms every manifest entry resolves to an object.

Do not commit a large media file merely for backpressure testing. Automated tests can create a temporary large object with deterministic content.

## 9. Transferability and conformance testing

The key proof is not merely that duplex works. It is that HTTP/2 works through a replaceable byte-stream provider.

Structure the integration suite around a transport-pair setup function:

```text
create transport pair
       │
       ├── gateway-side InnerTransport
       └── home-side InnerTransport
                    │
             run common suite
```

Checkpoint 1 supplies a Tokio-duplex pair. Checkpoint 2 will supply a WebSocket-backed pair and run the same common suite without copying or changing its protocol assertions.

The common suite must not mention `DuplexStream`, WebSocket messages, TCP, or machine addresses after setup returns its two I/O objects.

### 9.1 Unit tests

Test:

- Valid and invalid object IDs.
- Canonical path derivation.
- All three range forms.
- Boundary, empty-file, and one-byte-file cases.
- Overflow, malformed syntax, multiple ranges, and unsatisfiable ranges.
- Metadata, size, path, and content-type mismatches.
- Missing objects.

### 9.2 Common HTTP/2 transport suite

Run the real client and server over the provided transport pair and verify:

- Successful handshake and clean shutdown.
- Complete `GET`.
- Metadata-only `HEAD`.
- Beginning, middle, end-bounded, open-ended, and suffix ranges.
- Exact status, headers, content lengths, and response bytes.
- `400`, `404`, `405`, and `416` paths.
- Rejection of request bodies, query strings, incorrect authority, and noncanonical paths.
- Prompt failure when the underlying byte stream closes.

### 9.3 Flow-control tests

Verify:

- Large responses arrive in bounded chunks.
- The server does not read an entire file ahead of HTTP/2 capacity.
- A stalled stream consumes at most its stream window.
- The larger connection window leaves capacity for other streams.
- A small JPEG completes while a large stream remains deliberately stalled.
- Nine simultaneous operations result in eight active streams and one bounded waiter.
- Memory use does not grow in proportion to a generated large object's size.

Add test-only instrumentation around file reads where necessary. Assert high-water marks rather than relying only on process RSS in automated tests.

### 9.4 Cancellation tests

Verify:

- Dropping a public response body drops the associated inner body.
- The client sends or causes an HTTP/2 stream reset.
- The server stops reading that object's file.
- The other active streams remain usable.
- A new request succeeds over the same HTTP/2 connection.

### 9.5 Public endpoint tests

Start the public server on an ephemeral port and test:

- Ready health response.
- Full and partial downloads.
- Public-to-inner status and header mapping.
- Slow public consumption.
- Public cancellation.
- `503` after deliberate inner transport failure.

## 10. Container packaging

Create one multi-stage image:

- Build with the pinned Rust toolchain.
- Copy only the executable and fixture object store into the runtime stage.
- Use a minimal Debian runtime image.
- Run under a fixed unprivileged UID and GID.
- Expose port `8080`.
- Set the container configuration defaults.
- Support a read-only root filesystem.
- Require no writable volume for this read-only proof of concept.

The documented run command should bind the port only to the intended LAN or loopback address, drop all capabilities, enable `no-new-privileges`, and use `--read-only`.

Compose is deferred because this checkpoint deliberately has only one process and no WebSocket connection between machines.

## 11. Implementation sequence

Implement in this order so each stage is independently testable:

1. Create the workspace and pin the toolchain.
2. Implement `ObjectId`, metadata, range resolution, and filesystem layout.
3. Add fixture generation documentation, committed fixtures, and integrity tests.
4. Implement the generic `InnerTransport` boundary and shared limits.
5. Implement the HTTP/2 server with `HEAD`, full `GET`, and ranged `GET`.
6. Implement explicit outbound HTTP/2 flow control and cancellation handling.
7. Implement the HTTP/2 client and streaming response type.
8. Create the duplex transport-pair test harness.
9. Complete the common protocol, concurrency, flow-control, and cancellation suite.
10. Add the runnable public server and bridge public requests to the inner client.
11. Add public endpoint and failure tests.
12. Add structured logs and graceful shutdown.
13. Add the non-root Docker image and manual commands.
14. Run manual throttling, cancellation, range, and memory checks.
15. Record the checkpoint results and make the HTTP/2 continuation decision.

Do not begin WebSocket implementation until the checkpoint decision is recorded.

## 12. Verification

Required automated commands:

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Required manual checks:

1. Start the runnable proof of concept.
2. Request `/health` and confirm inner HTTP/2 readiness.
3. Download the JPEG and MP4 and compare their SHA-256 digests.
4. Request beginning, middle, and ending ranges and compare bytes with the source fixtures.
5. Throttle the MP4 response and request the JPEG concurrently.
6. Cancel the throttled download and confirm a new request succeeds.
7. Exercise invalid IDs, missing objects, malformed ranges, and unsatisfiable ranges.
8. Terminate the inner transport in a test build and confirm fast `503` behavior.
9. Run the same checks against the container.
10. Observe memory during idle, stalled, concurrent, cancelled, and large temporary-object tests.

## 13. Acceptance criteria

Checkpoint 1 is complete when:

- The runnable service uses the real inner HTTP/2 client and server over bounded Tokio duplex I/O.
- Both HTTP/2 endpoints depend only on `InnerTransport`.
- No duplex-specific type leaks into the object, HTTP/2, or public-handler APIs.
- No WebSocket dependency exists yet.
- All connection creation is confined to orchestration code.
- The common conformance suite accepts abstract transport pairs.
- Checkpoint 2 can add a WebSocket-backed pair and reuse the suite unchanged.
- Complete and ranged downloads match the fixtures byte-for-byte.
- A stalled large stream does not block a small object.
- Cancelling one request stops only its corresponding home-side work.
- No layer buffers a complete object.
- Working memory stays bounded for objects much larger than the configured windows.
- All automated checks pass.
- The container runs as non-root with a read-only object store and root filesystem.
- The checkpoint report explicitly recommends either proceeding with HTTP/2-over-WebSocket or replacing the inner protocol before adapter work begins.

## 14. Decision record after implementation

Record the following measurements and observations in this document or a linked results document:

- Approximate HTTP/2 client and server implementation size.
- Complexity of explicit send and receive flow control.
- Idle and peak resident memory.
- File-read and buffered-byte high-water marks.
- Small-object latency while a large stream is stalled.
- Time for home-side work to stop after cancellation.
- Behavior after byte-stream failure.
- Whether standard HTTP methods, statuses, headers, and ranges simplified the design.
- Whether the transport boundary remained clean enough for the WebSocket adapter to be plug-compatible.

Proceed to Checkpoint 2 only if HTTP/2 provides enough value to justify its implementation and operational complexity.
