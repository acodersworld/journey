# Storage Interface Review Follow-Up Implementation Plan

**Status:** Ready for implementation  
**Created:** 25 September 2026  
**Scope:** The two outstanding findings from the `journey-storage` interface review

## 1. Goal

Finish the current storage-interface refactor by addressing two remaining
boundary problems:

1. Never expose internal storage error text through the HTTP/2 service.
2. Make an invalid object content type unrepresentable after validation.

This is a focused hardening increment. It does not change the accepted design
where the current GET implementation returns a complete `Bytes` payload, and
it does not add filesystem storage, ranges, limits, or new routes.

The existing test suite is currently being migrated to the new storage API.
That wider migration is separate, but the focused tests described here must be
added or updated as part of these changes.

## 2. Bounded public storage errors

### 2.1 Public response contract

Add one fixed response body for unexpected storage failures:

```rust
const STORAGE_ERROR_BODY: &[u8] = b"storage error\n";
```

For this slice, every error returned by these storage operations maps to the
same public response:

| Operation | HTTP status | Response body |
| --- | ---: | --- |
| Create PUT context | `500 Internal Server Error` | `storage error\n` |
| Append PUT data | `500 Internal Server Error` | `storage error\n` |
| Commit PUT | `500 Internal Server Error` | `storage error\n` |
| Read object | `500 Internal Server Error` | `storage error\n` |

Do not send `StoreError::to_string()`, operating-system errors, paths, keys,
temporary filenames, or database details to the remote peer.

The response body remains a static bounded value regardless of the size or
contents of the internal error. Restore `send_text_response` to accept
`&'static [u8]` and send it with `Bytes::from_static`; remove the dynamic
`body.to_vec()` allocation and the commented-out old signature.

### 2.2 Internal diagnostics

Before returning the fixed response, record the detailed error locally with
enough operation context to distinguish:

- PUT context creation;
- PUT body append;
- PUT commit; and
- GET lookup.

For the current prototype, `eprintln!` is sufficient and does not require a
logging dependency. Do not log payload bytes. Avoid interpolating an
unvalidated logical key directly into the message; if a key is included, use
debug formatting so control characters are escaped.

Keep the detailed storage error available for local diagnosis. Static public
responses do not require discarding the error source or replacing internal
errors with static strings.

### 2.3 Error types

The current string-based storage error may remain for this narrow increment.
The HTTP handler must treat it as internal-only.

A later filesystem increment should replace `String` with a typed
`StoreError` so the HTTP layer can safely distinguish invalid input, capacity,
temporary unavailability, corruption, and unexpected failure. That later
mapping may introduce bounded `400`, `413`, `503`, or `507` responses. Do not
infer those categories from error-message text in this slice.

## 3. Validated content type

### 3.1 Shared value type

Introduce a public `ContentType` value type in the storage-interface module.
It owns an `http::HeaderValue` and has a private field so callers cannot create
an invalid instance directly.

Expose only the operations needed by the service and storage backends:

```rust
#[derive(Clone, Debug)]
pub struct ContentType(HeaderValue);

impl ContentType {
    pub fn try_from_header(value: &HeaderValue) -> Result<Self, ContentTypeError>;
    pub fn as_header_value(&self) -> &HeaderValue;
}
```

`try_from_header` accepts a content type only when:

- the header value is non-empty; and
- `HeaderValue::to_str()` succeeds.

This deliberately validates the existing prototype contract, not complete
media-type grammar. Values such as `image/jpeg`, `video/mp4`, and
`application/octet-stream` remain valid. Full MIME parsing and parameter
normalization are not part of this slice.

Add a small typed `ContentTypeError` rather than returning a free-form string.
It needs `Debug`, `Display`, and `std::error::Error`; it does not need to expose
the rejected header bytes.

Re-export `ContentType` and `ContentTypeError` from `journey-storage`.

### 3.2 Interface changes

Replace unrestricted content-type strings at the storage boundary:

- `ObjectInterface::content_type` returns `&ContentType`.
- `StoreInterface::put_context` receives an owned `ContentType`.
- The in-memory `Object` stores `ContentType`.
- The in-memory `PutContext` stores `ContentType`.
- Fixture and example object constructors accept `ContentType`, or perform
  validation before constructing an object.

The HTTP PUT handler performs validation once:

1. Require exactly one `Content-Type` request header.
2. Pass it to `ContentType::try_from_header`.
3. Return the existing bounded `400 Bad Request` response if validation fails.
4. Move the validated `ContentType` into `store.put_context`.

The storage layer must not repeat HTTP header parsing for an ordinary valid
PUT.

The HTTP GET handler obtains the already validated content type from the
returned object and adds `content_type.as_header_value()` to the response. It
must not reconstruct a `HeaderValue` from a string or retain the current
runtime `500 invalid content type` branch.

Future filesystem parsing must create `ContentType` through the same validated
constructor. Invalid persisted metadata then becomes a storage/corruption
error from `get`, which maps to the fixed storage-error response rather than
an invalid `ObjectInterface` value.

### 3.3 In-memory construction invariant

Update the in-memory object's constructor so it cannot accept an arbitrary
content-type string. Initial objects supplied to `Store::new` must already
contain a validated `ContentType`.

After this change:

- a normal in-memory object can never contain an empty content type;
- it can never contain CR/LF header injection;
- it can never contain a header value rejected by `to_str`; and
- GET does not need defensive header reconstruction for in-memory objects.

Remove the test that deliberately constructs an invalid stored content type
and expects GET to detect it. Replace it with constructor/value-type tests
showing invalid values are rejected before an object can be created.

## 4. Focused tests

### 4.1 Public error disclosure tests

Add test-only storage implementations or failure injection that can fail each
operation independently with a distinctive internal message such as:

```text
secret internal storage detail /private/path
```

For context creation, append, commit, and GET lookup failures, assert:

- the response status is `500`;
- the body is exactly `storage error\n`;
- the body has the correct `Content-Length`;
- the internal message is absent from the response headers and body; and
- a failed PUT does not publish or replace an object.

The tests do not need to capture or assert stderr. Their purpose is to prove
the remote response boundary.

### 4.2 Content-type tests

Cover:

- creation from `image/jpeg` and round-tripping the same header value;
- rejection of an empty header value;
- rejection of a non-visible value for which `to_str()` fails;
- PUT with one valid content type succeeding;
- PUT with a missing, empty, duplicated, or invalid content type returning
  `400` without mutation;
- GET returning the exact validated content type stored by PUT; and
- example fixtures using validated `ContentType` values.

Do not add tests for full MIME grammar because that validation is explicitly
outside this slice.

### 4.3 Verification

After the separate API migration has made the existing tests compile, run:

```bash
cargo check -p journey-storage --all-targets
cargo test -p journey-storage
```

Also run:

```bash
git diff --check
git diff --cached --check
```

Remove the current trailing whitespace in `storage_in_memory.rs`. Do not run
`cargo fmt`, `cargo fmt --check`, `rustfmt`, or another automated formatter.

## 5. Acceptance criteria

This follow-up is complete when:

1. No storage error text is copied into an HTTP response.
2. Every unexpected storage failure returns the same fixed, bounded `500`
   response and retains detailed local diagnostics.
3. `send_text_response` once again accepts only static bounded bodies and uses
   `Bytes::from_static`.
4. Objects and PUT contexts store `ContentType`, not an unrestricted string.
5. PUT is the single HTTP validation point for request content types.
6. GET uses the stored validated header value without reparsing.
7. Invalid persisted content type data must surface as a storage error rather
   than an invalid object.
8. Focused tests prove both non-disclosure and the content-type invariant.
9. The completed test suite and checks pass without automated formatting.

