# HTTP/2 Object Management Routes Implementation Plan

**Status:** Implemented  
**Created:** 25 September 2026  
**Scope:** Expose the existing STAT, LIST, and DELETE storage operations through
the `journey-storage` HTTP/2 service

## 1. Goal

Extend the current HTTP/2 service from GET/PUT to the complete object-management
surface already supported by `StoreInterface`:

```text
GET     /objects/<key>
HEAD    /objects/<key>
PUT     /objects/<key>
DELETE  /objects/<key>
GET     /objects?prefix=<prefix>
```

This increment is an HTTP adapter change. It must use the existing storage
interface rather than adding storage-specific behavior to the service. Existing
GET and PUT behavior, including HTTP/2 flow control and streaming, remains
unchanged except for routing it through the new resource-aware dispatcher.

## 2. Scope boundaries

Implement:

- `HEAD` for metadata-only object lookup;
- `DELETE` with idempotent success semantics;
- prefix-filtered, paginated `GET /objects` listing;
- stateless Base64URL continuation tokens;
- JSON list responses;
- collection-specific and object-specific method handling;
- bounded public errors with private diagnostic logging;
- focused HTTP/2 integration tests; and
- updated HTTP examples and route documentation.

Defer:

- authentication and authorization;
- conditional requests such as `If-Match`;
- delimiter-based pseudo-directories;
- range requests and partial object reads;
- multipart operations;
- object versions, hashes, ETags, and timestamps;
- snapshot-consistent listing;
- XML or S3 wire compatibility; and
- filesystem or SQLite implementation work.

## 3. Route model and dispatch

Classify the URI path before dispatching on the request method.

| Resource | Path | Supported methods |
| --- | --- | --- |
| Object collection | exact `/objects` | `GET` |
| Individual object | `/objects/<non-empty-key>` | `GET`, `HEAD`, `PUT`, `DELETE` |
| Invalid empty key | exact `/objects/` | none |
| Unknown resource | any unrelated path | none |

The query string does not affect route classification. Existing item requests
may continue to contain query parameters without changing the object key; only
the collection LIST route parses and validates its query string.

Responses for routing failures are:

- a recognized collection with an unsupported method returns `405 Method Not
  Allowed`, `Allow: GET`, and the existing bounded method-error body;
- a recognized object with an unsupported method returns `405 Method Not
  Allowed`, `Allow: GET, HEAD, PUT, DELETE`, and the bounded method-error body;
- `/objects/` returns the existing `400 Bad Request` invalid-key response; and
- an unrelated path returns `404 Not Found` with the bounded not-found body.

For a HEAD request, send the same status and applicable headers but suppress
the response body and finish the stream with the header block. This applies to
routing and validation errors as well as storage lookup errors.

The `Allow` header is resource-specific. It describes valid operations for the
targeted URL, not every method implemented anywhere in the server. This follows
normal HTTP `405` semantics and S3's distinction between collection/bucket and
individual-object operations.

## 4. HEAD object metadata

### 4.1 Request

```http
HEAD /objects/photos/image.jpg HTTP/2
```

Validate the key using the same `Key::new` path used by GET and PUT, then call:

```rust
store.stat(&key).await
```

Do not implement HEAD by calling GET. The purpose of the operation is to obtain
metadata without asking a future filesystem backend to open or return the
payload.

### 4.2 Responses

For an existing object:

```http
HTTP/2 200
content-type: image/jpeg
content-length: 428901
```

Send the response headers with `end_stream = true`. The `Content-Length` value
describes the object payload a GET would return; HEAD itself sends no DATA.

Return:

- `400` with no response body for an invalid or empty key;
- `404` with no response body for `StoreErrorKind::NotFound`; and
- `500` with no response body for any other storage error.

Omit `Content-Length` and `Content-Type` on HEAD error responses. Log storage
diagnostic details locally, but do not expose `StoreError::Display` text in
response headers.

Add a small empty-response helper rather than using `send_text_response`, since
HTTP HEAD responses must not send an error body.

## 5. DELETE object

### 5.1 Request

```http
DELETE /objects/photos/image.jpg HTTP/2
```

Validate the key and call:

```rust
store.delete(&key).await
```

### 5.2 Responses

A successful storage call always returns:

```http
HTTP/2 204 No Content
```

Send no DATA body and omit `Content-Length`, as required for a `204` response.
The same response is used when the key was already absent. This preserves the
current `delete() -> Result<(), StoreError>` contract and makes retry after an
uncertain network result unambiguous: the requested final state has been
reached.

Return:

- `400` with the bounded invalid-key body for an invalid or empty key; and
- `500` with the bounded storage-error body when the storage operation fails.

Do not perform STAT before DELETE. Such a check would add work, race with the
delete, and still could not provide an atomic existed/did-not-exist result.

## 6. LIST collection

### 6.1 Request contract

```http
GET /objects?prefix=photos%2F&limit=100&cursor=cGhvdG9zL2ltYWdlMTAwLmpwZw HTTP/2
```

Supported query parameters are:

| Parameter | Required | Meaning |
| --- | --- | --- |
| `prefix` | yes | Return keys starting with this decoded UTF-8 string; `prefix=` lists all keys |
| `limit` | no | Requested nonzero page size; defaults to `1000` |
| `cursor` | no | Unpadded Base64URL token containing the preceding page's last key |

Parsing rules:

1. Split and validate the raw query without using form semantics.
2. Verify that every `%` is followed by exactly two hexadecimal digits, then
   percent-decode each value exactly once as UTF-8. This explicit validation is
   required because a permissive decoder may otherwise preserve malformed `%`
   text rather than rejecting it.
3. Treat `+` as a literal plus; clients use `%20` for a space.
4. Reject missing `prefix`, unknown parameters, and duplicate parameters.
5. Parse `limit` as a decimal `usize`, reject zero and overflow, then construct
   `NonZeroUsize`. The backend remains responsible for clamping it to the
   configured maximum page size.
6. Decode `cursor` using the Base64URL no-padding alphabet. Reject `=` padding,
   standard Base64 `+` or `/`, malformed input, invalid UTF-8, and an empty key.
7. Construct a `Key` from the decoded value and require that it starts with the
   supplied prefix. A mismatch returns `400 Bad Request` rather than silently
   broadening or changing the continuation.

The cursor is opaque by contract but not encrypted or security-sensitive. It is
exactly the reversible Base64URL representation of a key and contains no prefix,
page size, expiry, signature, or server-side session identifier. A continuation
request must repeat the prefix. Changing this representation later is allowed to
be a breaking protocol change; do not add a version wrapper now.

After validation, build the existing storage request without moving `prefix`
before it is cloned into the storage cursor:

```rust
let cursor = cursor.map(ListCursor::new);
let request = ListRequest::new(prefix, cursor, requested_limit);
```

The HTTP module is in the same crate as the cursor's crate-visible constructor,
so the storage interface does not need to expose cursor internals publicly.

### 6.2 Response contract

A successful response is JSON:

```http
HTTP/2 200
content-type: application/json
content-length: <serialized byte count>
```

```json
{
  "objects": [
    {
      "key": "photos/image.jpg",
      "content_type": "image/jpeg",
      "size": 428901
    }
  ],
  "next_cursor": "cGhvdG9zL2ltYWdlLmpwZw"
}
```

The response rules are:

- `objects` is always present and follows the backend's lexicographic order;
- `key` and `content_type` are JSON strings;
- public `size` is the internal `ObjectMetadata::payload_length()` in bytes;
- `next_cursor` is the unpadded Base64URL encoding of the returned
  `ListCursor::last_key()`;
- `next_cursor` is omitted, not set to `null`, on the final page; and
- no separate `is_truncated` field is returned because the presence of
  `next_cursor` already communicates that state.

Use private serializable HTTP DTOs rather than adding serialization derives or
HTTP field names to the storage-domain types. This keeps `payload_length` as the
precise internal name while exposing the familiar object-storage name `size`.

Serialize the bounded page into a `Vec<u8>`/`Bytes`, set its exact
`Content-Length`, and send it through the existing flow-controlled
`send_payload` function. The storage page-size cap bounds the number of objects
in this allocation.

Any storage failure returns the existing bounded `500 storage error` response.
The validated `ContentType` invariant permits its header value to be converted
to a JSON string. Handle any unexpected conversion or serialization failure as
an internal `500`, with details logged only locally.

### 6.3 Pagination behavior

The token identifies an inclusive lexicographic start position, not an object
that must still exist. If the referenced object is deleted between requests,
the next page starts at the first key after its position. The token is global
and can be combined with a different prefix on the next request.

LIST does not provide a snapshot. Concurrent changes have ordinary
start-position behavior:

- an object inserted before the cursor will not appear in later pages;
- an object inserted at the cursor position may appear in the next page;
- an object inserted after the cursor may appear in a later page; and
- an object deleted before its later page is read will not appear.

Document this behavior and do not add locks spanning multiple requests.

## 7. Dependencies and internal structure

Add only the lightweight dependencies required by the HTTP representation:

```toml
base64 = "0.22"
percent-encoding = "2"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
```

Use `base64::engine::general_purpose::URL_SAFE_NO_PAD` for both cursor encoding
and decoding. Use `percent-encoding` for query values so literal plus signs are
not converted into spaces.

Keep all route parsing, query parsing, cursor conversion, and JSON DTOs private
to `http2_storage_service`. Do not expand the public storage API or re-export
wire-format types.

Refactor the service into focused helpers for:

- route classification;
- collection-query parsing;
- cursor encoding and decoding;
- empty/header-only responses;
- JSON responses; and
- the HEAD, LIST, and DELETE handlers.

Continue using the existing text-response helper for non-HEAD errors. Preserve
the existing `ServiceError` boundary for HTTP construction and HTTP/2 transport
failures; expected validation and storage failures are converted into responses
inside the handlers.

## 8. Test plan

### 8.1 HEAD tests

- existing object returns `200`, exact content type, payload content length, and
  an empty received body;
- zero-length object returns `Content-Length: 0` and no body;
- missing object returns bodyless `404`;
- invalid/empty key returns bodyless `400`;
- STAT is used without invoking GET; and
- internal STAT failure returns bodyless `500` without leaking diagnostic text.

### 8.2 DELETE tests

- deleting an existing object returns `204` and a subsequent GET returns `404`;
- deleting an absent object returns the identical `204` response;
- repeated deletion remains `204`;
- success omits `Content-Length` and has no body;
- invalid/empty key returns `400`; and
- internal DELETE failure returns bounded `500` without leaking diagnostic
  text or removing the object.

### 8.3 LIST success tests

- `prefix=` lists the full catalogue in lexicographic order;
- a non-empty prefix filters results exactly;
- an empty result returns `{"objects":[]}` with no `next_cursor`;
- omitted limit requests `1000` and remains subject to backend clamping;
- explicit limit produces multiple pages without duplicates;
- listed metadata maps payload length to `size` correctly;
- an intermediate response includes `next_cursor`;
- the final response omits `next_cursor`;
- cursor continuation still works after the referenced object is deleted;
- Unicode keys and keys containing spaces, plus signs, slashes, and punctuation
  survive query and cursor round trips; and
- JSON content type and serialized content length are exact.

### 8.4 LIST validation tests

Return `400` for:

- missing prefix;
- duplicate prefix, limit, or cursor;
- an unknown parameter;
- malformed percent escapes or invalid decoded UTF-8;
- zero, negative, non-numeric, or overflowing limit values;
- standard/padded or otherwise malformed Base64 cursor input;
- a cursor that decodes to invalid UTF-8 or an empty key; and
- reuse of a cursor with a different prefix, preserving the cursor's global
  key position while filtering by the newly supplied prefix.

Also verify that a literal `+` remains `+` while `%20` becomes a space.

### 8.5 Routing and regression tests

- collection errors advertise only `Allow: GET`;
- object errors advertise `Allow: GET, HEAD, PUT, DELETE`;
- HEAD routing and validation errors send no DATA body;
- unrelated paths return `404` rather than an invalid-key result;
- `/objects/` retains its invalid-empty-key response;
- object GET requests with an existing ignored query continue to work;
- existing GET payload, segmentation, backpressure, and concurrency tests pass;
- existing PUT streaming, replacement, error, and concurrency tests pass; and
- multiple HEAD, LIST, GET, PUT, and DELETE streams can share one HTTP/2
  connection without blocking unrelated streams.

Extend the HTTP test `FailureStore` with independent `Stat`, `List`, and
`Delete` failure modes so each handler's error mapping and redaction can be
tested directly.

## 9. Implementation sequence

1. Add serialization, Base64URL, and percent-decoding dependencies.
2. Introduce route classification and route-specific method dispatch while
   keeping existing GET/PUT tests green.
3. Add the empty-response helper and implement HEAD.
4. Implement DELETE with unconditional `204` success.
5. Add strict collection-query parsing and cursor conversion.
6. Add private LIST response DTOs, serialization, and flow-controlled sending.
7. Extend failure injection and add the new integration tests.
8. Update HTTP examples and relevant roadmap/interface documentation to mark
   STAT, LIST, and DELETE routes as implemented.
9. Run the crate tests and workspace checks. Do not run `cargo fmt`,
   `cargo fmt --check`, `rustfmt`, or any other automated source formatter.

## 10. Acceptance criteria

The increment is complete when:

- all five object operations are reachable through the documented HTTP/2
  routes;
- HEAD reads metadata without retrieving payload contents;
- DELETE is safely repeatable and returns `204` for absent objects;
- LIST provides strict prefix-filtered pagination using the agreed stateless
  Base64URL cursor;
- final LIST pages omit `next_cursor`;
- route-specific `405` responses advertise accurate method sets;
- no new response exposes private storage diagnostic text;
- all new and existing HTTP/storage tests pass; and
- no storage-interface or WebSocket transport change is required.
