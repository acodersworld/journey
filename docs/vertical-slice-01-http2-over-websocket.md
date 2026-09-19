# Vertical Slice 01: HTTP/2 over WebSocket Object Download

**Status:** Proof-of-concept specification  
**Updated:** 19 September 2026  
**Scope:** Validate HTTP/2 as the protocol carried inside WebSocket, then prove the WebSocket bridge on a LAN  
**Parent architecture:** [Journey Architecture](architecture.md)

## 1. Purpose

Determine whether HTTP/2 is the right protocol to carry inside the already-selected WebSocket transport, without building the website around it. The slice is deliberately split into two checkpoints:

1. Run the object protocol over a direct Tokio byte stream created with `tokio::io::duplex`.
2. Insert a WebSocket bridge between the same HTTP/2 client and server without changing either HTTP/2 endpoint or the object protocol.

The completed slice proves that:

- Home initiates one persistent WebSocket connection to the web-facing server.
- The WebSocket is adapted into an ordered asynchronous byte stream.
- AWS acts as an HTTP/2 client inside that byte stream.
- Home acts as the corresponding HTTP/2 server.
- Public HTTP requests are bridged to independent inner HTTP/2 object requests.
- File bodies stream with bounded memory and HTTP/2 backpressure.

This slice runs on two physical machines on a trusted internal network. It uses preinstalled JPEG and MP4 objects and implements only read operations.

Checkpoint 1 succeeds when the HTTP/2 object API demonstrates streaming, ranges, concurrency, cancellation, and bounded backpressure over `tokio::io::duplex`. Checkpoint 2 succeeds when `curl` or a browser can retrieve and seek within the same preinstalled objects across two machines after the WebSocket bridge is inserted, while the HTTP/2 and object-service code remains unchanged.

## 2. The decision being tested

A temporary JSON `get`/`file` WebSocket protocol would prove connectivity but not the production machinery needed for concurrent video, cancellation, and bounded streaming. WebSocket itself is a fixed architectural choice. The open question is what protocol should run inside it.

Checkpoint 1 isolates that decision from WebSocket mechanics:

- Does HTTP/2 provide a clean object API for `GET`, `HEAD`, ranges, status, and metadata?
- Can multiple HTTP/2 streams coexist independently?
- Does slow consumption apply backpressure without unbounded buffering?
- Can one request be cancelled without losing other streams?
- Are the implementation and failure behavior simpler than owning a custom mux protocol?

Checkpoint 2 then validates only the transport adaptation:

- Can WebSocket safely behave as `AsyncRead + AsyncWrite`?
- Can the endpoint that accepted the outer connection become the inner HTTP/2 client?
- Do reconnects return the system to a clean state?

HTTP/2 message semantics, multiplexing, flow control, and cancellation are delegated to the established Rust HTTP/2 implementation instead of being recreated as a custom protocol.

## 3. Deliberately excluded

- HTML pages or a browser application
- Uploads, `PUT`, deletion, or object listing
- AWS deployment or internet exposure
- TLS/WSS
- HTTP Basic or Ed25519 authentication
- React or Node.js
- Nginx or public HTTP/2
- SQLite, posts, or an object catalog
- Server-side object caching
- Image processing or video transcoding
- HLS or DASH
- Production monitoring or CI

Plain `ws://` is a proof-of-concept shortcut. The server must bind only to the trusted LAN and must never be port-forwarded or deployed publicly in this form. Production authentication occurs before the inner HTTP/2 handshake and does not change the data path being tested here.

## 4. Topology and protocol roles

```text
Machine A: gateway host

curl or browser
      |
      | HTTP/1.1 GET/HEAD/Range on trusted LAN
      v
+--------------------------------------+
| gateway container                    |
|                                      |
| Public test HTTP endpoints           |
| HTTP/2 client                        |
| WebSocket byte-stream adapter        |
| WebSocket endpoint                   |
+------------------^-------------------+
                   |
                   | one persistent ws:// connection
                   | initiated from Machine B
                   |
Machine B: home host

+------------------|-------------------+
| home-agent container                 |
|                                      |
| WebSocket byte-stream adapter        |
| HTTP/2 server                        |
| /data/objects mounted read-only      |
+--------------------------------------+
```

The outer roles and inner roles are intentionally different:

```text
Outer WebSocket:  Home initiates  ─────────────► Gateway accepts
Inner HTTP/2:     Home is server  ◄───────────── Gateway is client
```

The direction in which the TCP/WebSocket connection was created does not determine the roles of the protocol carried inside it.

## 5. Rust components

The proof of concept is one Cargo workspace with shared validation code and two binaries:

```text
journey/
├── Cargo.toml
├── crates/
│   └── object-model/
└── apps/
    ├── gateway/
    └── home-agent/
```

The expected library set is deliberately small:

- Tokio for the asynchronous runtime and I/O.
- Axum and Hyper primitives for the public test endpoints and WebSocket upgrade.
- A Rust WebSocket implementation plus a byte-stream adapter.
- The `h2` crate for inner HTTP/2 client and server roles.
- `http` types for requests, responses, methods, headers, and status codes.
- SHA-2 for fixture import and test verification.
- `tracing` for console diagnostics.
- `thiserror` for bounded internal error types.

The HTTP/2 endpoints accept a generic Tokio I/O value satisfying `AsyncRead + AsyncWrite + Unpin + Send + 'static`. They must not import or depend upon WebSocket types. Checkpoint 1 supplies the ends of `tokio::io::duplex`; checkpoint 2 supplies the same interface while bridge tasks connect local duplex streams to WebSocket.

The WebSocket bridge remains an explicit proof-of-concept component. Its buffering, shutdown, ping/pong, and maintenance characteristics must be verified. A small project-owned bridge requires focused unit and integration tests before production use.

## 6. Preinstalled objects

Before starting the test, place at least one JPEG and one MP4 in the home object store. Every object is identified by the SHA-256 digest of its exact bytes:

```text
sha256:<64 lowercase hexadecimal characters>
```

An illustrative layout is:

```text
home-data/
└── objects/
    └── 01/
        └── 23/
            ├── 0123...cdef.blob
            └── 0123...cdef.json
```

The bounded sidecar metadata is:

```json
{
  "object_id": "sha256:0123...",
  "size": 842193,
  "content_type": "image/jpeg"
}
```

Fixture preparation calculates SHA-256 before publishing the file. At runtime, the home agent validates identifier syntax, derived path, metadata, and file size. The object directory is read-only inside the container.

## 7. Pluggable byte-stream boundary

The inner HTTP/2 implementation is parameterized over a generic Tokio byte stream:

```rust
AsyncRead + AsyncWrite + Unpin + Send + 'static
```

For checkpoint 1, the HTTP/2 client and server receive the two ends returned by `tokio::io::duplex`. This is an in-process integration harness, not the final network topology. It proves the inner protocol independently and makes failures attributable either to HTTP/2/object semantics or, later, to the WebSocket bridge.

For checkpoint 2, each HTTP/2 endpoint receives one end of a local duplex stream. A bridge owns the other end and pumps bytes to and from the WebSocket:

```text
HTTP/2 endpoint <-> DuplexStream <-> bridge <-> WebSocket
```

No HTTP/2 handler, route, request, response, or object-storage code may change between the two checkpoints. Only connection setup supplies a different byte stream.

### WebSocket bridge

The bridge must preserve the byte-stream behavior expected by `h2` while using only bounded binary WebSocket messages underneath.

### Read behavior

- Accept binary messages only after the WebSocket upgrade.
- Present their payload bytes consecutively without exposing message boundaries to HTTP/2.
- Retain at most one configured small WebSocket payload plus bookkeeping.
- Consume ping, pong, and close frames as WebSocket control traffic rather than inner bytes.
- Treat text messages and invalid fragmentation as protocol errors.
- Return end-of-stream or an error consistently when the WebSocket closes.

### Write behavior

- Divide the byte stream into bounded binary WebSocket messages, initially targeting 64 KiB payloads.
- Preserve byte order exactly.
- Propagate backpressure instead of accumulating an unbounded write queue.
- Map flush and shutdown behavior consistently.
- Never enable WebSocket compression for already-compressed media or opaque HTTP/2 bytes.

WebSocket message boundaries have no meaning to HTTP/2. A single HTTP/2 frame may span several WebSocket messages, and one WebSocket message may contain parts of several HTTP/2 frames.

The bridge must be tested independently using arbitrary byte sequences, randomized WebSocket message boundaries, small read buffers, early closure, and bidirectional traffic. The duplex capacity and WebSocket write queue must both be explicitly bounded.

## 8. Connection establishment

### Checkpoint 1: inner protocol harness

1. Create a bounded `tokio::io::duplex` pair.
2. Give one end to the HTTP/2 client and the other to the HTTP/2 server.
3. Perform the HTTP/2 handshakes and continuously drive both connection futures.
4. Exercise the complete read-only object API through the resulting client handle.

This harness is retained as an integration test after checkpoint 2 is implemented.

### Checkpoint 2: WebSocket transport

1. The home agent connects to `ws://<machine-a-lan-address>:8080/internal/storage`.
2. The gateway accepts the WebSocket upgrade.
3. Each side creates a bounded local duplex pair and starts a bridge between one end and the WebSocket.
4. The gateway starts an HTTP/2 client handshake over its other duplex end.
5. Home starts an HTTP/2 server handshake over its other duplex end.
6. Each side continuously drives its HTTP/2 connection future.
7. The gateway reports the storage connection as ready after the inner handshake succeeds.

There is no separate JSON `hello` or `ready` protocol. The HTTP/2 preface and settings exchange establish the inner protocol. In production, Basic and Ed25519 authentication will occur after WebSocket upgrade but before step 3 or 4.

## 9. Public test interface

Machine A exposes ordinary HTTP endpoints for manual testing.

### `GET /objects/{object_id}`

The gateway validates the object ID, opens a new inner HTTP/2 stream, and sends:

```http
GET /objects/sha256/0123... HTTP/2
```

It maps the inner status and bounded response headers to the public response and forwards DATA payloads incrementally.

### `HEAD /objects/{object_id}`

Returns object existence and metadata without a body. The gateway uses an inner `HEAD` request.

### `GET /objects/{object_id}` with `Range`

The gateway initially accepts one valid byte range and forwards it to home:

```http
Range: bytes=4194304-8388607
```

Home returns `206 Partial Content`, exact `Content-Length`, and exact `Content-Range`. Invalid or unsatisfiable ranges return the appropriate bounded error response.

### `GET /health`

Reports whether the outer WebSocket is present and the inner HTTP/2 connection is ready. This endpoint is diagnostic only.

## 10. Inner home object API

The inner server supports only:

- `GET` for a complete object.
- `HEAD` for object metadata.
- `GET` with one byte range.

It rejects:

- All other methods.
- Multiple or malformed ranges.
- Non-canonical paths.
- IDs that are not strict lowercase SHA-256 identifiers.
- Request bodies.
- Excessive headers.
- Arbitrary authorities, URLs, query strings, or filesystem paths.

The initial implementation omits `PUT`, `DELETE`, and `LIST`. These are not required to validate the transport and would expand the consequences of a gateway compromise.

## 11. Streaming and backpressure

The data path is:

```text
Home file
    ↓ bounded reads
Home HTTP/2 DATA frames
    ↓
WebSocket byte-stream adapter
    ↓ bounded WebSocket messages
Gateway HTTP/2 response body
    ↓ bounded forwarding
Public HTTP response
    ↓
curl or browser
```

The gateway must not collect the complete inner response body. It releases inner HTTP/2 receive capacity only as data is forwarded or placed into a strictly bounded buffer. A deliberately slow public client must therefore slow home reads instead of increasing gateway memory in proportion to file size.

The fixture's content address is validated when it is installed at home. For the proof of concept, downloaded bytes are verified after each test with SHA-256. The gateway does not delay the public stream to re-hash and buffer the complete object.

## 12. Multiplexing and cancellation

Every public object request receives its own inner HTTP/2 stream. At least two simultaneous downloads must be tested:

```text
inner stream 1: large MP4 or throttled range
inner stream 3: small JPEG
```

The JPEG must complete without waiting for the complete MP4 transfer.

If the public client cancels a request, the gateway drops or resets the corresponding inner response stream. Home stops work for that stream while other streams and the outer WebSocket remain usable.

Concurrency is deliberately capped at a small configured value. Multiplexing is not permission for unbounded streams, file handles, tasks, or buffers.

## 13. Failure behavior

### No ready home connection

- Public object requests fail quickly with `503 Service Unavailable`.
- The gateway does not queue requests indefinitely.

### Missing or invalid object

- Invalid public identifiers return `400 Bad Request` without opening an inner stream.
- A well-formed missing object returns `404 Not Found`.

### Individual stream failure

- Only the affected public request fails.
- The remaining HTTP/2 streams and WebSocket connection continue if the inner connection remains valid.

### WebSocket or inner connection failure

- All active inner streams fail.
- The gateway discards the unusable HTTP/2 client handle.
- Home reconnects with bounded exponential backoff.
- A fresh WebSocket receives a fresh HTTP/2 connection; streams are never resumed across connections.
- Public clients may retry safe `GET` or `HEAD` requests.

### Process restart

- Preinstalled home objects remain unchanged.
- Container restart policies restart both programs.
- Home reconnects and performs a new inner HTTP/2 handshake.

## 14. Container deployment

Both programs are built as Linux containers from the same Cargo workspace. A multi-stage Dockerfile compiles the binaries and copies only the required binary and runtime files into separate minimal final targets.

### Machine A

The gateway container:

- Publishes port `8080` on Machine A's explicit LAN address.
- Runs as a non-root user with a read-only root filesystem where practical.
- Drops unnecessary Linux capabilities and uses `no-new-privileges`.
- Restarts automatically unless deliberately stopped.

Illustrative mapping:

```yaml
ports:
  - "192.168.1.10:8080:8080"
```

Machine A's firewall permits the port only from the trusted LAN.

### Machine B

The home-agent container:

- Publishes no ports.
- Connects outbound to `ws://<machine-a-lan-address>:8080/internal/storage`.
- Mounts `./home-data/objects:/data/objects:ro`.
- Runs as a non-root user with a read-only root filesystem.
- Drops unnecessary Linux capabilities and uses `no-new-privileges`.
- Restarts automatically unless deliberately stopped.

Each machine can build its image manually from the same source checkout. No registry or CI pipeline is required.

## 15. Manual test sequence

1. Run the in-process duplex integration harness and complete all object, range, concurrency, cancellation, slow-consumer, and bounded-memory tests.
2. Confirm the harness uses the same HTTP/2 client, server, and object handlers intended for the containers.
3. Preinstall one JPEG and one sufficiently large MP4 at home and record their object IDs.
4. Start the gateway container on Machine A and the home-agent container on Machine B.
5. Confirm that the WebSocket bridge and inner HTTP/2 handshakes complete without changing the inner endpoint code.
6. Request `HEAD` for both fixtures.
7. Download the JPEG and compare its SHA-256 digest with the source.
8. Download the MP4 and compare its SHA-256 digest with the source.
9. Request beginning, middle, and ending ranges and compare each byte-for-byte with the source.
10. Throttle an MP4 download and request the JPEG concurrently; confirm the JPEG finishes promptly.
11. Cancel the throttled request; confirm the JPEG and a new request still work on the same connection.
12. Request invalid, missing, and unsatisfiable objects/ranges and verify bounded errors.
13. Stop home during concurrent requests and confirm they fail without hanging.
14. Restart home and confirm a fresh inner HTTP/2 session becomes ready.
15. Restart the gateway and confirm home reconnects.
16. Measure both containers' memory during idle, full download, range download, concurrency, cancellation, throttling, and reconnect tests.

## 16. Acceptance criteria

The slice is complete when:

- The HTTP/2 object protocol passes first over a bounded `tokio::io::duplex` harness.
- Switching to WebSocket changes only connection setup and bridge code.
- Home always initiates the outer WebSocket connection.
- AWS/gateway acts as the inner HTTP/2 client and home as the inner server.
- WebSocket message boundaries are invisible to HTTP/2.
- `GET`, `HEAD`, and one-range `GET` work for preinstalled objects.
- Complete and partial downloads match the original bytes.
- Response bodies stream without whole-object buffering at the gateway.
- A slow public client produces bounded memory and backpressure.
- A small object completes while a large transfer remains active.
- Cancelling one inner stream does not close the other streams or outer WebSocket.
- Reconnecting creates a clean new HTTP/2 session.
- Both containers run non-root, only Machine A publishes a LAN port, and the home volume is read-only.
- The implementation is clearly marked unsafe for public deployment until WSS and authentication are added.

## 17. Decision after the slice

This slice is intended to make an architectural decision, not merely demonstrate a download. After it succeeds, record:

- Whether HTTP/2 earned its place as the inner protocol before considering bridge complexity.
- Adapter implementation size and testability.
- Peak and steady-state memory.
- Behavior under slow consumers and cancellation.
- HTTP/2 reconnect and failure complexity.
- Whether the standard semantics materially simplify the gateway and home handler.

If HTTP/2 behaves well in checkpoint 1 and the bridge remains reliable and understandable in checkpoint 2, retain HTTP/2-over-WebSocket as the production private transport. If HTTP/2 itself is disproportionate, compare it against WebSocket-native multiplexing such as `websock-mux`, Yamux, or bounded `get_range` messages before spending effort on the production bridge. If only the bridge is problematic, keep the HTTP/2 endpoint boundary and replace the bridge implementation.
