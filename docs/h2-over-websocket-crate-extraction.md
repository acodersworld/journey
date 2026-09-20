# HTTP/2-over-WebSocket Crate Extraction Review

**Status:** Source hardening complete; Docker and sustained RSS verification remain
**Updated:** 20 September 2026
**Reviewed component:** `crates/journey-websocket`  
**Related plan:** [Vertical Slice 01 Implementation Plan](vertical-slice-01-implementation-plan.md)

**Working checklist:** [HTTP/2-over-WebSocket Hardening Checklist](h2-over-websocket-hardening-checklist.md)

## 1. Conclusion

The current implementation proves that an HTTP/2 connection can be carried over one persistent WebSocket between the gateway and home processes. The adapter is appropriately small, the two-process prototype operates successfully, and the complete workspace test suite passes.

The four original extraction blockers have now been completed:

1. The bridge is genuinely full duplex under backpressure.
2. The client and server expose symmetric streaming `h2` primitives.
3. Incoming WebSocket messages are bounded at handshake time.
4. Direct adapter, lifecycle, backpressure, cancellation, and HTTP/2-over-WebSocket tests are present.

Subsequent review also added explicit HTTP/2 connection limits, coordinated
client request readiness, and non-waiting server request admission. A full
application request queue now resets only the excess stream with
`REFUSED_STREAM` while the driver continues polling existing streams.

The intended extracted crate is specifically an **HTTP/2-over-WebSocket** crate. It should know about WebSocket transport, the byte-stream conversion, HTTP/2 handshakes, and connection lifecycle. It should not know about Journey routes, object storage, files, caching, or gateway policy.

## 2. What the prototype already demonstrates

The current implementation establishes the essential architecture:

```text
Gateway HTTP/2 client
        │
        │ Tokio byte stream
        ▼
Local duplex endpoint
        │
WebSocket bridge
        │
        │ persistent WebSocket
        ▼
WebSocket bridge
        │
Local duplex endpoint
        │
        ▼
Home HTTP/2 server
```

The implementation already provides:

- One persistent WebSocket reused for multiple inner requests.
- HTTP/2 handshakes over a WebSocket-backed byte stream.
- Binary WebSocket messages carrying opaque stream bytes.
- Separation of WebSocket control traffic from HTTP/2 data.
- Rejection of text messages.
- Configurable local duplex capacity.
- Configurable outbound WebSocket chunk size.
- A generic server callback that keeps Journey routing outside the bridge.
- Coordinated lifetime between the WebSocket bridge and HTTP/2 connection driver.
- Home-initiated connection establishment and gateway-side acceptance.
- Containerized gateway and home processes.

The thinness of the bridge is expected. Its job is not to parse HTTP/2. It only preserves one ordered sequence of bytes while converting between Tokio byte-stream I/O and WebSocket binary messages.

## 3. Correct crate boundary

The extracted crate should own:

- The local bounded duplex stream.
- Conversion between the duplex byte stream and WebSocket binary messages.
- HTTP/2 client and server handshakes.
- HTTP/2 connection-driver lifecycle.
- Coordinated bridge and HTTP/2 shutdown.
- WebSocket message and buffer limits.
- Transport-level errors and connection health.
- WebSocket subprotocol identification and versioning, if adopted.

Journey should continue to own:

- `/objects`, `/ping`, `/pong`, and all other routes.
- HTTP methods and application-specific validation.
- File lookup and byte-range behavior.
- HTTP/2 response-body production and consumption.
- Object streaming and caching.
- Authentication performed before starting the inner HTTP/2 session.
- Reconnection policy and replacement of an old home session.
- Logging, metrics, and operational policy.

The low-level WebSocket bridge can remain an internal module of the extracted crate. It does not need to become a separate published crate unless another concrete use case later requires a generic WebSocket byte stream.

## 4. Resolved issue: full-duplex backpressure

### Current behavior

The bridge uses one `tokio::select!` loop. It waits for either:

- An incoming WebSocket message, or
- Bytes read from the local duplex stream.

After a branch wins, it awaits the corresponding write inside that branch.

For example:

```text
incoming WebSocket message
        │
        ▼
await writing its bytes into the duplex stream
```

While that write is blocked, the loop does not poll the opposite direction. The same issue occurs if sending an outgoing WebSocket message blocks.

### Why it matters

HTTP/2 is full duplex even when an application appears to be transferring data in one direction. Control frames, window updates, resets, pings, request headers, and response data can need to move in the opposite direction.

A stalled write in one direction must not prevent the other direction from carrying HTTP/2 control traffic. Otherwise, backpressure can produce a connection-level deadlock.

### Required design

Run two independent pumps:

```text
Pump A: local byte stream ──> WebSocket writer
Pump B: WebSocket reader  ──> local byte stream
```

Requirements:

- Each pump makes progress independently.
- Termination or failure of either pump cancels the other.
- The WebSocket sink has a single owner.
- Ping, pong, and close handling can reach that sink without creating an unbounded queue.
- A bounded control channel may be used if the reader must ask the writer to emit a pong or close response.
- Duplex EOF starts a WebSocket close handshake where practical.
- WebSocket close or EOF shuts down the local write half so HTTP/2 observes EOF.
- The function does not return while an orphaned pump remains active.

This is the most important transport correction before extraction.

## 5. Resolved issue: application-specific client API

### Current behavior

The current `Client::request(path)` convenience API:

- Constructs a `GET` request internally.
- Hard-codes `home.internal`.
- Accepts only a path rather than a complete HTTP request.
- Collects the complete response body.
- Converts the body to UTF-8 text.
- Returns only a status and `String`.

This works for the prototype's small `/ping` and `/pong` responses, but it cannot support the intended media workload. It also embeds Journey assumptions in a crate intended for reuse.

### Required design

Expose normal streaming HTTP/2 primitives. A client session should provide access to an `h2::client::SendRequest<Bytes>` or a thin wrapper that preserves equivalent behavior:

- Caller supplies a complete `http::Request<()>`.
- Caller waits for HTTP/2 request readiness.
- Caller receives the complete `http::Response<h2::RecvStream>`.
- Response bodies remain streaming and binary.
- The crate does not assume UTF-8.
- The crate does not collect bodies.
- The crate does not invent an authority, path, method, or headers.
- HTTP/2 flow-control handles remain available to the application.
- Multiple cloned request handles can create independent streams.

Journey can provide a local convenience wrapper for its own object API, but that wrapper should not live in the extracted transport crate.

## 6. Resolved issue: inbound WebSocket limits

### Current behavior

The adapter configuration bounds:

- The local duplex capacity.
- The buffer used to create outbound WebSocket messages.

It does not bound incoming WebSocket message size. By the time the bridge receives a `Message::Binary`, the WebSocket implementation may already have allocated and assembled the complete message.

Rejecting an oversized message inside the bridge is therefore too late to protect memory.

### Required design

Configure limits when the WebSocket client or server is created:

- Maximum incoming message size.
- Maximum incoming frame size.
- Maximum write buffer size.
- Maximum queued write size, where supported.
- WebSocket compression disabled for opaque HTTP/2 bytes.

The crate has two possible API strategies:

1. Provide connection and acceptance helpers that construct `tokio-tungstenite` with the required `WebSocketConfig`.
2. Accept an already-upgraded WebSocket but require the caller to supply evidence/configuration that the documented limits were applied.

The first option is safer and easier to use correctly. The second is useful when another HTTP framework owns the upgrade. If both are supported, the documentation must clearly state that accepting an arbitrary preconstructed `WebSocketStream` cannot retroactively limit memory used while parsing a message.

## 7. Resolved issue: missing adapter tests

The current workspace test proves HTTP/2 over an in-process Tokio duplex pair. It does not directly exercise the WebSocket bridge.

The extracted crate needs its own tests that create a real in-memory or loopback WebSocket pair and run the production bridge code.

### Byte-transport tests

Verify:

- Bytes travel correctly in both directions simultaneously.
- WebSocket message boundaries disappear from the byte stream.
- One byte-stream write may become multiple WebSocket messages.
- Several WebSocket messages appear as one continuous byte stream.
- Partial reads and writes preserve every byte in order.
- Randomized payload sizes and message boundaries preserve data.
- A stalled receiver applies backpressure without unbounded allocation.
- Backpressure in one direction does not stop the opposite direction.
- Text messages produce a protocol error.
- Ping and pong remain control traffic.
- Close frames produce byte-stream EOF.
- Byte-stream EOF produces an orderly WebSocket close where possible.
- Failure of either pump terminates the entire adapter.
- Invalid zero-sized configuration values are rejected.
- Oversized incoming messages are rejected by handshake-time limits.

### HTTP/2-over-WebSocket tests

Run the real `h2` client and server through the production adapter and verify:

- Client and server handshakes complete.
- Several sequential requests reuse one WebSocket.
- Multiple simultaneous requests use independent HTTP/2 streams.
- A stalled large response does not block a small response.
- Cancelling one stream leaves the connection and other streams usable.
- Binary response bodies stream without aggregation or UTF-8 conversion.
- HTTP/2 flow-control window updates can travel while application data is backpressured.
- WebSocket closure fails active streams promptly.
- Dropping the final session handle terminates the driver cleanly.

## 8. Symmetric public API

The current server API is protocol-generic while the client API is explicitly HTTP/2 and application-specific. A dedicated HTTP/2-over-WebSocket crate should expose symmetric client and server sessions.

An illustrative API is:

```rust
let client_session = h2_websocket::client(websocket, config).await?;
let mut sender = client_session.sender();

let request = http::Request::builder()
    .version(http::Version::HTTP_2)
    .method(http::Method::GET)
    .uri("https://home.internal/objects/sha256:...")
    .body(())?;

let (response, request_body) = sender.send_request(request, true)?;
```

The corresponding server API is:

```rust
let mut server_session = h2_websocket::server(websocket, config).await?;

while let Some((request, responder)) = server_session.accept().await? {
    // Application-specific handling remains outside the crate.
}
```

The exact types may wrap `h2` values to coordinate driver lifetime. They must preserve streaming, flow-control, resets, concurrency, and access to terminal connection errors.

Recommended session behavior:

- `ClientSession::sender()` returns a cloneable streaming request handle.
- `ServerSession::accept()` yields normal HTTP/2 requests and responders.
- Both sessions own or reference a driver task.
- Both expose a way to await terminal connection status.
- Dropping a session initiates coordinated transport shutdown.
- Background errors are observable programmatically rather than only written with `eprintln!`.

## 9. HTTP/2 request readiness

Before creating a stream, an `h2` client must wait for its request handle to
become ready. `ClientSender` now provides that behavior without serializing
response bodies: it holds a shared gate only while waiting for readiness and
creating the stream, then returns the response future and streaming request
body handle to the caller.

The shared gate is important because a cloned `h2::client::SendRequest` has its
own pending-stream state. The `h2` crate intentionally permits one request to
wait behind the peer's concurrent-stream limit. Allowing every application
clone to own that pending state would allow each clone to queue another stream.
All `ClientSender` clones therefore coordinate through the same underlying
handle, limiting the session to the one pending stream managed by `h2`.

This mutex is not held while awaiting response headers or streaming either
body, so independent active streams still progress concurrently.

## 10. Multiplexing and streaming in Journey

The current Journey gateway stores the client behind a mutex and holds that mutex for the entire request. It also uses a client method that collects the complete body. Consequently, the prototype proves connectivity and reuse but does not yet prove HTTP/2 multiplexing or media streaming.

After the extracted crate exposes a cloneable streaming sender:

- Lock only long enough to obtain or replace a session handle.
- Clone the sender for each public request.
- Do not hold a global mutex while awaiting a response body.
- Forward `RecvStream` DATA incrementally.
- Release HTTP/2 receive capacity as the public response advances.
- Drop or reset the inner stream when the public client disconnects.
- Apply a separate Journey-level concurrency limit.

These changes belong to Journey rather than the extracted crate, but the crate API must make them possible.

## 11. WebSocket subprotocol and compatibility

A standalone crate creates a wire protocol that may be used by independently upgraded peers. Define how compatible peers identify it.

Prefer a WebSocket subprotocol token such as:

```text
h2-over-websocket-v1
```

The final token should be checked for standards and registry conflicts before publication. Both peers should offer or require the same token during the HTTP upgrade. A mismatch must fail before starting the HTTP/2 preface.

The compatibility policy should state:

- WebSocket binary payloads are concatenated into one HTTP/2 byte stream.
- Text messages are invalid.
- WebSocket compression is disabled.
- One WebSocket carries exactly one HTTP/2 connection.
- Reconnection creates a new HTTP/2 connection; streams are not resumed.
- Control frames retain normal WebSocket semantics.
- Future incompatible framing changes require a new subprotocol version.

## 12. Suggested extracted repository

An initial repository could contain:

```text
h2-over-websocket/
├── Cargo.toml
├── LICENSE
├── README.md
├── src/
│   ├── lib.rs
│   ├── bridge.rs
│   ├── client.rs
│   ├── server.rs
│   ├── config.rs
│   └── error.rs
├── tests/
│   ├── bridge.rs
│   ├── h2_session.rs
│   └── backpressure.rs
└── examples/
    ├── client.rs
    └── server.rs
```

Internal responsibilities:

| Module | Responsibility |
|---|---|
| `bridge` | Two independent byte pumps and coordinated shutdown |
| `client` | HTTP/2 client handshake, sender access, and driver lifecycle |
| `server` | HTTP/2 server handshake, request acceptance, and driver lifecycle |
| `config` | Duplex, chunk, WebSocket, and HTTP/2 connection limits |
| `error` | Structured setup, transport, HTTP/2, protocol, and shutdown errors |

Examples should demonstrate binary streaming rather than string-only ping responses.

## 13. Extraction sequence

Complete the work in this order:

1. Replace the single bridge loop with independent bidirectional pumps.
2. Add bridge-level lifecycle and backpressure tests.
3. Add handshake-time WebSocket size limits.
4. Replace `Client::request(path)` with a streaming HTTP/2 client session.
5. Add a symmetric HTTP/2 server session API.
6. Make driver errors and shutdown observable through session APIs.
7. Add concurrent HTTP/2 and cancellation tests over real WebSockets.
8. Define and enforce the WebSocket subprotocol token.
9. Move Journey-specific request construction and response collection into Journey.
10. Confirm Journey works against the local extracted crate path.
11. Create the independent repository with license, README, examples, and tests.
12. Point Journey at a pinned Git revision or published crate version.
13. Only then remove the in-tree crate.

During extraction, keep the wire behavior stable so the home and gateway can be migrated independently during development.

## 14. Extraction acceptance criteria

The crate is ready to move when:

- Its public purpose is explicitly HTTP/2 over WebSocket.
- The bridge supports independent progress in both directions.
- Every buffer and message-size limit is explicit and bounded.
- Client and server APIs are symmetric and streaming.
- No API assumes Journey paths, authorities, methods, UTF-8, or small bodies.
- Applications retain access to HTTP/2 flow control and stream cancellation.
- Client request readiness is handled correctly.
- Driver failures are observable without parsing logs.
- A WebSocket subprotocol identifies the wire protocol.
- Adapter and HTTP/2-over-WebSocket tests cover backpressure and cancellation.
- A full application request queue refuses only the excess stream without stopping the connection driver.
- HTTP/2 stream, connection-window, header-list, concurrency, and request-queue limits are explicit; stream and connection windows independently enforce the crate's supported nonzero range `1..=2^31 - 1`.
- Journey can perform concurrent requests without holding a global request mutex.
- Journey compiles and runs using the crate through an external path or Git dependency.
- The crate has a clear license, README, compatibility statement, and runnable examples.

Once these criteria are satisfied, moving the crate into its own repository is a low-risk change rather than an architectural rewrite.
