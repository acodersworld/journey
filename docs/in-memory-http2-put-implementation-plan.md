# In-Memory HTTP/2 PUT Implementation Plan

**Status:** Implemented  
**Created:** 24 September 2026  
**Scope:** The next minimal `journey-storage` slice after the read-only service

## 1. Goal

Extend the existing `journey-storage` crate and h2c example with a minimal
HTTP/2 `PUT` operation:

```http
PUT /objects/<logical-key>
```

The operation receives an HTTP/2 request body, creates or replaces one object
in the process-local store, and makes the completed object immediately
available through the existing `GET` operation.

This slice defines protocol and service-state behavior. It covers:

- receiving an HTTP/2 body through `h2::RecvStream`;
- applying HTTP/2 receive flow control correctly;
- publishing an object only after its complete body has arrived;
- atomically replacing the value associated with one key; and
- using GET and PUT concurrently on one HTTP/2 connection.

It deliberately retains complete objects in memory. It does not yet implement
the production filesystem design.

## 2. Explicit scope

This slice implements:

- `PUT /objects/<logical-key>`;
- S3-style unconditional create-or-replace behavior;
- a required `Content-Type` request header;
- an optional `Content-Length` request header;
- incremental receipt of HTTP/2 DATA frames;
- a mutable, concurrency-safe in-memory `Store`;
- atomic publication of a completely received object;
- updated method discovery through `Allow: GET, PUT`;
- PUT support in the existing h2c example; and
- in-process HTTP/2 tests using Tokio duplex streams.

This slice does not implement:

- filesystem storage or `.part` files;
- the 512-byte Journey object-container header;
- crash durability or persistence across process restarts;
- upload, object, retained-store, or PUT-specific concurrency limits;
- create-only or replace-only modes;
- `If-Match`, `If-None-Match`, ETags, or version checks;
- checksums or content-derived object IDs;
- byte ranges or `HEAD`;
- SQLite, manifests, listing, deletion, or recovery tools;
- authentication, TLS, or WebSocket integration; or
- changes to `apps/home`, `apps/gateway`, or `journey-websocket`.

The current implementation has no memory limits. Restrict access to
controlled environments until upload and retained-store limits are added. A
peer can otherwise exhaust process memory with one large upload or many
retained objects.

## 3. HTTP contract

### 3.1 Route and key

The PUT route uses the same exact-key interpretation as GET:

```http
PUT /objects/<logical-key>
```

The service takes the request URI path, removes the exact `/objects/` prefix,
and uses the remainder as the logical key. For this slice:

- an empty key is invalid;
- no percent decoding is added;
- no path normalization is added;
- the query component is ignored because routing uses `Uri::path()`; and
- the existing `Key` validation remains the single source of key validity.

An invalid or empty key returns `400 Bad Request` with a bounded text body.
GET retains its existing `404 Not Found` behavior for unknown and empty keys.

### 3.2 Required metadata

Every PUT requires one valid `Content-Type` header. Its value is stored as the
object's content type and returned by later GET responses.

The request must contain exactly one non-empty `Content-Type` value. HTTP and
h2 parsing already reject values that cannot be represented as an HTTP header.
The service rejects a missing, duplicated, or empty value with `400 Bad
Request` before reading or publishing the object.

Change `Object` to store the content type as `http::HeaderValue`, and return
`&HeaderValue` from its accessor. This preserves the already validated value
without converting it through an unrestricted string. Update the constructor,
fixtures, example helper, and tests to supply a `HeaderValue`.

`Content-Length` is optional. HTTP/2 does not use HTTP/1.1
`Transfer-Encoding: chunked`; request bodies arrive as DATA frames and finish
at `END_STREAM`, whether or not a length was declared.

The `h2` crate validates the HTTP/2 content-length invariant while receiving a
stream. A malformed value, more DATA than declared, or `END_STREAM` before the
declared length is satisfied is a stream protocol error. The service must
propagate that failure and must not publish the pending object. When no
`Content-Length` is present, it accepts DATA until `END_STREAM`.

Do not use a declared length to allocate the complete buffer eagerly. Grow the
request-local body buffer only as DATA is actually received.

### 3.3 Upsert semantics

An ordinary PUT is unconditional:

| State at publication | Result |
| --- | --- |
| Key absent | Insert the object and return `201 Created` |
| Key present | Replace the complete object and return `200 OK` |

Replacement changes both payload and content type. It does not merge metadata
or preserve any part of the previous object.

The response has an empty body and ends on its header block. It includes:

```text
Content-Length: 0
```

Conditional creation and replacement are deferred. A later slice may add the
standard HTTP forms:

```text
If-None-Match: *    create only
If-Match: *         replace only
If-Match: "etag"    replace only if the current version matches
```

They must not be approximated with query parameters or non-atomic prechecks in
this slice.

### 3.4 Method handling

GET and PUT are the only accepted methods. Any other method returns:

```http
405 Method Not Allowed
Allow: GET, PUT
```

The error response body remains a small bounded constant.

## 4. Mutable in-memory store

### 4.1 Shared state

Refactor `Store` so all of its clones share one mutable catalogue. Use an
asynchronous reader/writer lock around the map so:

- unrelated and concurrent GET lookups can proceed under read access;
- a completed PUT takes write access only for final publication; and
- replacing a map entry is atomic from the perspective of other handlers.

Do not hold the store lock while receiving the request body. A slow uploader
must not prevent GETs or other completed PUTs from accessing the catalogue.

The current constructor continues to accept initial objects for the example.
Existing lookup behavior continues to return a cheap clone of the immutable
`Bytes` payload handle.

Use `tokio::sync::RwLock` and enable Tokio's `sync` feature for the crate.
`Store::new` remains synchronous because it constructs unshared state. Change
`Store::get`, `Store::len`, and `Store::is_empty` to asynchronous methods;
callers and tests await them rather than taking a blocking lock inside Tokio
request tasks.

### 4.2 Publication operation

Add this public result type and store operation:

```rust
pub enum PutOutcome {
    Created,
    Replaced,
}

pub async fn put(&self, object: Object) -> PutOutcome
```

While holding write access, `put`:

1. inserts it when the key is absent;
2. replaces the existing map entry when the key is present; and
3. returns an outcome distinguishing `Created` from `Replaced`.

The HTTP service uses this outcome to choose `201` or `200`.

The operation must not expose the map or its lock publicly. Keep synchronization
and replacement semantics inside `Store` so later storage backends can change
without spreading locking decisions through the HTTP handler.

### 4.3 Concurrent PUTs

Two PUT handlers may receive bodies for the same key concurrently. Each body
is accumulated independently. Whichever handler acquires the write lock and
publishes last becomes the stored object.

A GET may observe the old object or either completely published new object. It
must never observe a partly received payload, a mixture of two payloads, or
new contents paired with the old content type.

## 5. Receiving a PUT body

The service handles a valid PUT in this order:

1. Extract and validate the logical key.
2. Read and validate the required `Content-Type` header.
3. Retain the `RecvStream` from the request body.
4. Create an empty request-local mutable byte buffer.
5. Await each DATA frame from `RecvStream::data()`.
6. Append that frame to the request-local buffer.
7. Release exactly that frame's receive-window capacity only after the append
   succeeds.
8. Continue until the peer sends `END_STREAM`.
9. Freeze the complete buffer into `Bytes` and construct the new `Object`.
10. Atomically upsert it into `Store`.
11. Send `201 Created` or `200 OK` according to the store outcome.

Although the service receives the body incrementally, the complete payload is
retained in RAM because this slice uses an in-memory store. HTTP/2 flow control
limits bytes still owned by the transport; it does not limit the growing
application buffer or the retained store.

If DATA receipt fails, the peer resets the stream, or the HTTP/2 connection
fails, discard the local buffer and return a typed service error. Do not
modify the existing object for that key and do not attempt to send a success
response.

An empty body is valid and publishes an object whose payload length is zero.

## 6. Service errors and responses

Extend the service error model only for failures that prevent the service from
forming or transmitting an appropriate HTTP response. Expected client input
problems should be converted to bounded HTTP responses instead of bubbling out
as internal errors.

The initial response table is:

| Condition | Status | Store changed |
| --- | ---: | --- |
| New key and complete valid body | `201` | Yes |
| Existing key and complete valid body | `200` | Yes |
| Empty or invalid key | `400` | No |
| Missing or invalid `Content-Type` | `400` | No |
| Unknown GET key | `404` | No |
| Unsupported method | `405` | No |
| HTTP/2 body or connection failure | Stream/connection failure | No |

Do not return internal lock details, buffer state, or transport error details
in response bodies.

## 7. h2c example

Extend the existing `h2c_get_server` example in place and retain its filename
for this slice so existing invocations continue to work.

The listener, h2 handshake, request task model, initial embedded image and
video, and loopback default remain unchanged. Each accepted request tuple is
still passed to the same shared `Service`; the service now dispatches GET and
PUT.

Document a manual round trip similar to:

```bash
curl --http2-prior-knowledge \
  -X PUT \
  -H 'Content-Type: image/jpeg' \
  --data-binary @new-image.jpg \
  http://127.0.0.1:8081/objects/uploaded.jpg

curl --http2-prior-knowledge \
  http://127.0.0.1:8081/objects/uploaded.jpg \
  --output downloaded.jpg

cmp new-image.jpg downloaded.jpg
```

Add a replacement example that uploads different bytes and a different
content type to the same key, then verifies the new GET response.

Do not add Docker, TLS, HTTP/1.1 upgrade, or WebSocket setup to this example.

## 8. Tests

Keep tests below the TCP and WebSocket boundaries by using
`tokio::io::duplex` with an h2 client and server.

### 8.1 Store tests

Cover:

- constructing the initial catalogue;
- asynchronous exact-key lookup;
- inserting a new object and receiving `Created`;
- replacing an object and receiving `Replaced`;
- replacement changing payload and content type together; and
- clones observing the same mutations.

### 8.2 HTTP/2 PUT tests

Cover:

- PUT of a new key returning `201` and an empty response body;
- GET immediately returning the uploaded bytes, content type, and length;
- PUT of an existing key returning `200`;
- GET returning only the replacement object;
- an empty PUT body;
- PUT without `Content-Length`, completed by `END_STREAM`;
- missing `Content-Type` returning `400` without changing the store;
- an empty key returning `400` without changing the store;
- malformed or mismatched `Content-Length` producing an h2 stream failure and
  leaving the store unchanged;
- a reset or interrupted request body leaving an existing value unchanged;
- an unsupported method returning `405` and `Allow: GET, PUT`;
- GET and PUT completing concurrently on one connection; and
- two concurrent PUTs to one key leaving exactly one complete submitted
  object as the final value.

The test h2 client must send request bodies through `SendStream`, reserve and
await send capacity where necessary, and mark only the final DATA frame as
end-of-stream. Tests reading response bodies must continue releasing receive
capacity.

For the concurrent same-key test, do not assert which request wins unless the
test explicitly controls publication order. Assert that the final object is
one complete candidate, with its matching content type.

### 8.3 Verification commands

Run:

```bash
cargo check -p journey-storage --all-targets
cargo test -p journey-storage
```

Do not run `cargo fmt`, `cargo fmt --check`, `rustfmt`, or another automated
source formatter. Preserve the repository's manual formatting.

## 9. Acceptance criteria

This slice is complete when:

1. The existing h2 service supports GET and unconditional PUT without knowing
   how the HTTP/2 connection was established.
2. PUT receives DATA incrementally and releases receive capacity only after
   each frame has been copied successfully.
3. No object becomes visible before its complete request body reaches
   `END_STREAM`.
4. A new key returns `201`; replacement returns `200`.
5. Replacement atomically changes payload and content type.
6. Failed or interrupted uploads leave the prior store state unchanged.
7. GET and PUT operate concurrently on a single HTTP/2 connection.
8. The h2c example demonstrates an upload followed by a byte-identical GET.
9. The crate remains independent of TCP, WebSocket, application, filesystem,
   and database concerns in its library code.
10. The lack of memory limits and persistence is clearly documented as unsafe
    for untrusted or production use.

## 10. Later increments

After this protocol slice succeeds:

1. Add explicit per-upload, in-flight, and retained-store limits while
   in-memory storage remains in use.
2. Add conditional PUT using `If-None-Match` and `If-Match` with atomic store
   checks.
3. Replace in-memory accumulation and retention with streaming filesystem
   writes to server-named `.part` files.
4. Introduce the fixed 512-byte Journey container and atomic durable
   publication.
5. Make GET stream bounded file reads rather than retained `Bytes`.
6. Add byte ranges, `HEAD`, SQLite indexing, manifests, and recovery tools.
7. Wire the unchanged HTTP/2 service boundary to the home WebSocket session.
