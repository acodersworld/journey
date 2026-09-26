# In-Memory HTTP/2 Range GET Implementation Plan

**Status:** Ready for implementation  
**Created:** 26 September 2026  
**Scope:** One byte-range GET across the `journey-storage` interface, HTTP/2
service, h2c example, and local browser console

## 1. Goal

Let a client retrieve part of an object without returning the rest of its
payload. Use the HTTP `Range` header on `GET /objects/<key>`, as S3 does, and
represent the selected bytes as an offset and size inside the storage result.
The current in-memory backend must select a `Bytes` slice without copying its
payload. The read result must contain metadata and bytes from one object
version even if a PUT replaces the key concurrently.

Keep complete GET, metadata-only HEAD, PUT, DELETE, and LIST behavior. This
increment supports one range per request. It does not add a filesystem backend,
ETags, `If-Range`, multipart ranges, or conditional requests.

## 2. Storage interface and in-memory backend

Add these public values to `journey-storage` and re-export them from `lib.rs`:

```rust
pub enum ReadRange {
    Closed { start: u64, end: u64 },
    From { start: u64 },
    Suffix { length: u64 },
}

pub struct ReadSpan {
    offset: u64,
    size: u64,
}

pub enum GetResult<O> {
    Found(ReadObject<O>),
    Unsatisfiable { complete_length: u64 },
}
```

Expose `ReadSpan::offset()` and `ReadSpan::size()` as copied `u64` accessors.
Add `selected_span: Option<ReadSpan>` and `ReadObject::selected_span()`;
`None` means a complete read, while
`Some(span)` means a successful Range request, including one that happens to
select the whole object. Retain `ReadObject::new(metadata, object)` as the
complete-read constructor and add `ReadObject::with_selected_span` for a
ranged result.

Change the trait operation to:

```rust
fn get(
    &self,
    key: &Key,
    range: Option<ReadRange>,
) -> impl Future<Output = Result<GetResult<Self::Object>, StoreError>> + Send;
```

`None` returns the complete object. For `Some(range)`, the store resolves the
request against the object's complete payload length and returns either the
selected bytes or `Unsatisfiable` with that length. A missing key remains a
`StoreErrorKind::NotFound` error. This typed result gives the HTTP layer the
length needed for `Content-Range: bytes */<length>` without a separate STAT
request. It also avoids a STAT/GET race with a concurrent replacement.

Resolve the range while holding the in-memory store's read lock. Clone the
object's `Bytes` and use `Bytes::slice` for the selected payload. Return full
`ObjectMetadata` together with the selected object and resolved `ReadSpan`;
release the lock before sending the response. A later PUT may change the key,
but it cannot change the bytes or length captured by that GET.

Resolution rules for an object of length `total` are:

| Request | Result |
| --- | --- |
| No range | All `total` bytes; complete-read result. |
| `Closed { start, end }`, with `start < total` and `end >= start` | Offset `start`; size `min(end, total - 1) - start + 1`. |
| `From { start }`, with `start < total` | Offset `start`; size `total - start`. |
| `Suffix { length }`, with `length > 0` and `total > 0` | Offset `total - min(length, total)`; size `min(length, total)`. |
| Any range on an empty object, `start >= total`, or zero suffix | `Unsatisfiable { complete_length: total }`. |

Reject `Closed { end < start }` in the HTTP parser before calling the store;
the store also returns `StoreErrorKind::InvalidRequest` if a direct caller
passes it.
Use checked arithmetic where converting indices to `usize`; do not allocate or
copy a full payload to produce a slice. Adapt existing trait implementations
and test fakes to the new signature and result type.

## 3. HTTP/2 Range contract

For an object GET, read at most one `Range` header. Accept these `bytes`
forms, with zero-based, inclusive positions:

```http
Range: bytes=0-1023
Range: bytes=1048576-
Range: bytes=-65536
```

Parse decimal positions as `u64`, rejecting overflow. Treat the range unit
case-insensitively and trim optional whitespace around the header value. An
unrecognized range unit is ignored, yielding an ordinary complete GET. A
malformed `bytes` range, reversed closed bounds, comma-separated multiple
ranges, or duplicate `Range` fields returns the existing bounded `400` text
error. A syntactically valid but unsatisfiable range returns `416`.

For a satisfiable request, send the selected bytes with:

```http
HTTP/2 206
accept-ranges: bytes
content-type: <stored type>
content-length: <selected size>
content-range: bytes <offset>-<offset + size - 1>/<complete length>
```

Return `206` even when the selected span covers the entire object. A complete
GET remains `200` with the full `Content-Length` and gains
`Accept-Ranges: bytes`. An unsatisfiable range returns `416`,
`Accept-Ranges: bytes`, and `Content-Range: bytes */<complete length>`, with a
bounded text error body. A missing object with a valid Range retains `404`;
internal store errors retain the existing bounded `500` response. A `416`
response sends only its bounded error body, never selected object bytes.

HEAD continues to call STAT, returns the complete object length, and ignores
`Range` entirely, including malformed or duplicate fields. Add
`Accept-Ranges: bytes` to successful HEAD responses. This follows general HTTP
Range semantics for GET rather than S3's special HEAD behavior.

Pass the selected `Bytes` to the existing flow-controlled `send_payload`
function. It must reserve capacity only for selected bytes and stop on stream
cancellation as it does for complete GET. Preserve the existing `64 KiB` DATA
segment bound.

## 4. Example, browser console, and documentation

Add one `curl --http2-prior-knowledge -H 'Range: bytes=0-99'` example to the
h2c server's startup guidance and the workspace README. Show the expected
`206`, `Content-Range`, and 100-byte payload.

In the local browser console, show a Range field for object GET. Send its
value as the upstream `Range` header through the Python proxy; do not send it
for HEAD, PUT, DELETE, or LIST. Forward `Accept-Ranges` and `Content-Range` in
the proxy response so the console can display them. A partial binary GET
remains downloadable using the existing download link. Update the console's
implementation document with the new control and a range acceptance check.

Revise the interface document's deferred-range paragraph to describe this
implemented in-memory slice and distinguish it from future filesystem
streaming. Do not claim full S3 protocol compatibility.

## 5. Tests and acceptance

Use the existing in-process `h2::client`/`h2::server` test harness. Assert
exact status, selected bytes, content type, `Content-Length`, `Content-Range`,
and `Accept-Ranges` for beginning, middle, final-byte, open-ended, suffix,
past-EOF end, and full-covering ranges. Test an empty object, start at EOF,
start beyond EOF, zero suffix, malformed or overflowing positions, reversed
bounds, unknown range unit, duplicate headers, and multiple ranges.

Test storage results directly: complete GET, each range form, unsatisfiable
length, and a captured ranged result after replacing the same key. Confirm
that captured metadata, selected span, and bytes still agree. Test HEAD with
both valid and malformed Range headers and confirm full-length `200` headers
with no body.

Manually run the h2c example and compare a ranged video or image download to
the corresponding source-file slice. Repeat through the Python browser proxy
and confirm that the console displays the upstream `206` and range headers.
Run `cargo check -p journey-storage --locked` and
`cargo test -p journey-storage --locked`. Do not run `cargo fmt`, `rustfmt`, or
any automated source formatter in this repository.

The wire behavior is based on the [S3 GetObject Range contract](https://docs.aws.amazon.com/AmazonS3/latest/API/API_GetObject.html)
and [HTTP Range semantics in RFC 9110](https://www.rfc-editor.org/rfc/rfc9110.html#name-range).
