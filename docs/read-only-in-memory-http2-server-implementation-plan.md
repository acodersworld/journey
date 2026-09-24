# Read-Only In-Memory HTTP/2 Server Implementation Plan

**Status:** Ready for implementation  
**Created:** 24 September 2026  
**Scope:** The first minimal, functioning object-server slice

## 1. Goal

Add a `journey-storage` library crate containing a fixed in-memory object
catalogue and a reusable HTTP/2 `GET` handler. Add a runnable cleartext HTTP/2
(`h2c`) Cargo example that owns TCP connection setup and makes the service
accessible with `curl`.

The library knows how to handle an already accepted HTTP/2 request. It knows
nothing about how the HTTP/2 connection was established:

```text
development example:
TCP -> h2 handshake -----------+
                               |
                               v
                    journey-storage handler
                               ^
                               |
future home application:       |
WebSocket ServerSession -------+
```

The existing gateway, home application, WebSocket transport, Docker
deployment, and file-backed validation fixtures remain unchanged in this
slice.

## 2. Explicit scope

This slice implements:

- one new reusable library crate;
- an immutable in-memory object catalogue;
- full-object HTTP/2 `GET` responses;
- bounded, flow-controlled HTTP/2 DATA sends;
- a standalone h2c example for manual use with `curl`; and
- library and handler tests that require neither TCP nor WebSocket.

This slice does not implement:

- `PUT` or any other mutation;
- `HEAD`;
- byte ranges;
- ETags;
- a health route;
- object listing;
- filesystem storage;
- the 512-byte object-container header;
- SQLite or JSON manifests;
- authentication or TLS;
- WebSocket integration;
- an HTML page;
- Docker packaging; or
- changes to `apps/home` or `apps/gateway`.

## 3. Workspace addition

Create:

```text
crates/journey-storage/
├── Cargo.toml
├── examples/
│   ├── assets/
│   │   ├── image.jpg
│   │   └── video.mp4
│   └── h2c_get_server.rs
└── src/
    └── lib.rs
```

Add `crates/journey-storage` to the workspace members.

The crate depends only on the primitives required for this slice:

- `bytes` for immutable, cheaply sliced payloads;
- `h2` for accepted requests, responses, DATA frames, and flow control;
- `http` for request and response types; and
- Tokio runtime and I/O primitives required by the library tests and example.

The library must not depend on:

- `journey-websocket`;
- Axum;
- TLS libraries;
- filesystem or database libraries; or
- any application crate in this workspace.

Tokio networking is used by the example target, not by library code.

## 4. In-memory catalogue

### 4.1 Public types

Expose a concrete `StaticObject` containing:

- a logical key;
- a validated content type; and
- an immutable `Bytes` payload.

Expose a concrete `StaticStore` constructed from a collection of
`StaticObject` values.

Construction validates the complete catalogue before making it available:

- a key must not be empty;
- a key must be unique within the catalogue; and
- a content type must be representable as a valid HTTP header value.

The constructor returns a typed error for invalid catalogue definitions rather
than panicking.

`StaticStore` provides exact logical-key lookup. A successful lookup returns a
cheap clone of the immutable object metadata and `Bytes` handle; it does not
copy the payload.

### 4.2 Initial catalogue

The example constructs the initial catalogue with exactly two keys:

```text
image.jpg
video.mp4
```

Their corresponding content types are:

```text
image/jpeg
video/mp4
```

The example embeds both payloads at compile time with `include_bytes!` and
wraps them with `Bytes::from_static`.

Add purpose-built, small, valid media assets for this example. Generate and
commit them once; generation tools must not become build-time or runtime
dependencies. Do not copy, modify, delete, or commit the existing untracked
large media under `deploy/media`.

## 5. HTTP/2 GET service

### 5.1 Public handler boundary

Expose `Http2GetService`, which owns or shares a `StaticStore` and handles one
already accepted HTTP/2 request through these transport-neutral h2 types:

```text
http::Request<h2::RecvStream>
h2::server::SendResponse<bytes::Bytes>
```

The handler does not:

- accept sockets;
- perform an h2 handshake;
- establish or upgrade a WebSocket;
- configure TLS;
- reconnect; or
- choose a listen address.

Those responsibilities belong to the program supplying accepted requests.

This signature intentionally matches the request and response tuple produced
by `journey_websocket::ServerSession`, without depending on that crate or its
type alias.

### 5.2 Route semantics

The only successful route form is:

```http
GET /objects/<logical-key>
```

The handler uses the URI path, removes the exact `/objects/` prefix, and looks
up the remaining text as an exact catalogue key.

For this fixed ASCII catalogue:

- do not add percent decoding;
- do not normalize the path;
- do not implement general S3 key validation; and
- ignore the query component when selecting the object because routing uses
  `Uri::path()`.

Response behavior is:

| Request | Result |
| --- | --- |
| `GET /objects/image.jpg` | `200` with the JPEG payload |
| `GET /objects/video.mp4` | `200` with the MP4 payload |
| `GET` for an empty or unknown key | `404` with a bounded text body |
| Any non-`GET` method | `405` with `Allow: GET` and a bounded text body |

A successful response includes:

- `Content-Type` from the catalogue; and
- the exact payload `Content-Length`.

Do not add `Accept-Ranges`, `Content-Range`, ETag, cache policy, or application
metadata in this slice.

### 5.3 Response streaming and flow control

Do not pass a complete large payload to `SendStream::send_data` without first
obtaining capacity. The handler sends each successful payload as zero-copy
`Bytes::slice` segments no larger than 64 KiB:

1. Determine the maximum next segment from the remaining payload and the
   64-KiB limit.
2. Reserve that amount of HTTP/2 send capacity.
3. Await positive capacity with `poll_capacity`.
4. Slice no more than the available capacity, configured segment limit, and
   remaining payload.
5. Send that slice immediately.
6. Set end-of-stream only on the final DATA segment.
7. Repeat until the payload is complete.

An empty payload ends the response on the header block without sending a DATA
frame.

If the peer resets the stream or the connection fails, stop sending and return
a typed service error. The library does not retry an individual response.

Small text error responses remain bounded constants. They may be sent in one
DATA frame because their complete size is known and strictly small.

## 6. h2c development example

Add the runnable Cargo example:

```text
journey-storage/examples/h2c_get_server.rs
```

The example owns all concrete network behavior:

- bind a Tokio `TcpListener`;
- default to `127.0.0.1:8081`;
- optionally accept an override from `JOURNEY_STORAGE_BIND`;
- accept TCP connections;
- perform a prior-knowledge `h2::server` handshake for each connection;
- accept HTTP/2 request streams;
- run bounded concurrent request tasks; and
- pass each request tuple to the shared `Http2GetService`.

Configure a small explicit maximum number of concurrent HTTP/2 streams and a
bounded maximum header-list size, following the conservative limits already
used by the transport prototype. Do not add HTTP/1.1 upgrade handling.

The example prints its bound address and the two available object URLs at
startup. Connection and request failures are logged without terminating the
listener unless binding itself fails.

Run it with:

```bash
cargo run -p journey-storage --example h2c_get_server
```

Download the fixed objects with:

```bash
curl --http2-prior-knowledge \
  http://127.0.0.1:8081/objects/image.jpg \
  --output image.jpg

curl --http2-prior-knowledge \
  http://127.0.0.1:8081/objects/video.mp4 \
  --output video.mp4
```

This listener is an unencrypted local development tool. Its loopback default
must not be changed to a public bind address in this slice.

## 7. Tests

### 7.1 Catalogue tests

Test the library directly without HTTP:

- valid catalogue construction;
- exact lookup for both known keys;
- unknown-key lookup;
- empty-key rejection;
- duplicate-key rejection;
- invalid content-type rejection; and
- payload lookup cloning without copying the underlying `Bytes` allocation.

### 7.2 HTTP/2 handler tests

Test `Http2GetService` over `tokio::io::duplex` with `h2::client` and
`h2::server`. Tests must not:

- bind TCP sockets;
- launch the h2c example;
- create WebSockets; or
- use fixture files at runtime.

Cover:

- known image and video responses with exact status, content type, content
  length, and payload bytes;
- unknown and empty keys returning `404`;
- non-`GET` methods returning `405` and `Allow: GET`;
- an empty object completing on response headers;
- a payload larger than 64 KiB completing across multiple DATA sends;
- a slow receiver with a deliberately small HTTP/2 window receiving the
  complete payload without eager unbounded send buffering; and
- two concurrent GET streams completing correctly on one HTTP/2 connection.

Raw h2 test clients must release receive-window capacity after consuming DATA
chunks so the test represents a correct peer.

### 7.3 Verification commands

Run:

```bash
cargo check -p journey-storage --all-targets
cargo test -p journey-storage
```

Do not run `cargo fmt`, `cargo fmt --check`, `rustfmt`, or another automated
source formatter. Preserve the repository's existing manual formatting.

## 8. Acceptance criteria

This slice is complete when:

1. `journey-storage` builds without depending on `journey-websocket` or an
   application crate.
2. Its library source contains no socket binding, TCP, TLS, WebSocket, or
   reconnect logic.
3. Its tests exercise catalogue and HTTP/2 GET behavior without TCP or
   WebSocket.
4. The h2c example starts on loopback and both documented `curl` commands
   reproduce the embedded assets byte for byte.
5. Known responses contain the exact content type and content length.
6. Unknown keys and unsupported methods return the documented bounded errors.
7. Large in-memory responses respect HTTP/2 send capacity and the 64-KiB
   segment limit.
8. The existing gateway/home validation applications and their deployment
   configuration are unchanged.

## 9. Later increments

After this slice succeeds, extend it one independently testable capability at
a time:

1. Add byte-range `GET` and then `HEAD`.
2. Replace or supplement the in-memory catalogue with filesystem-backed object
   readers.
3. Add the fixed 512-byte Journey container.
4. Add streaming `PUT` with `.part` publication.
5. Add SQLite, listing, JSON manifests, and recovery commands.
6. Pass `journey_websocket::ServerSession` request tuples to the unchanged
   HTTP/2 service handler from `apps/home`.

The first WebSocket integration changes only connection setup and application
wiring. It does not move TCP or WebSocket concerns into `journey-storage`.
