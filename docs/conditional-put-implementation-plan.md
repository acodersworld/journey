# Conditional PUT Implementation Plan

**Status:** Ready for implementation  
**Created:** 26 September 2026  
**Scope:** Add create-only and replace-only PUT conditions to the in-memory
store, HTTP/2 service, and local web console before implementing filesystem
storage

## 1. Goal

Keep ordinary `PUT /objects/<key>` as an unconditional create-or-replace
operation. Add two atomic conditions using standard HTTP wildcard headers:

| Request header | Required state at publication | Failed condition |
| --- | --- | --- |
| None | Any | Not applicable |
| `If-None-Match: *` | Key absent; create only | `412 Precondition Failed` |
| `If-Match: *` | Key present; replace only | `412 Precondition Failed` |

These wildcard forms follow [RFC 9110 conditional request semantics](https://www.rfc-editor.org/rfc/rfc9110.html#section-13.1).
An accepted PUT still returns the existing `200 OK`, `Content-Length: 0`, and
empty body, whether it created or replaced an object. A failed condition
leaves the previous object unchanged and returns a fixed
`precondition failed\n` text body. Do not add ETags or version-matching PUT in
this slice.

## 2. HTTP request handling

In the PUT handler, parse `If-Match` and `If-None-Match` before creating a PUT
context or receiving the body. Accept exactly one occurrence of either
header, with the value `*` after trimming HTTP optional whitespace. Reject
both headers together, duplicate occurrences, empty values, and any value
other than `*` with the existing bounded `400 Bad Request` response. Other
methods keep their current behavior.

Pass the parsed condition to the store, stream the request body through the
PUT context using the existing HTTP/2 flow-control path, and call the final
store `put`. The authoritative condition check happens at final publication,
after the body is received. This avoids a race with another PUT or DELETE
while the body is streaming. Map only `StoreErrorKind::PreconditionFailed` to
`412`; keep other storage errors on their existing error path. Do not expose
private store error details in the HTTP response.

## 3. Store interface and publication

Add a public `PutCondition` enum with `Unconditional`, `CreateOnly`, and
`ReplaceOnly`, and add `PreconditionFailed` to `StoreErrorKind`. Keep the
existing `Conflict` kind separate for other conflicts. Apply the agreed
key-in-context change at the same time:

```rust
fn put_context(
    &self,
    key: Key,
    content_type: ContentType,
    condition: PutCondition,
) -> impl Future<Output = Result<Self::PutContext, StoreError>> + Send;

fn put(
    &self,
    context: Self::PutContext,
) -> impl Future<Output = Result<(), StoreError>> + Send;
```

The context owns the validated key, content type, condition, and accumulated
payload. Update the in-memory store and all `StoreInterface` test doubles and
callers to this signature. In-memory `put` takes the B-tree write lock once,
checks presence and the condition, and either publishes the complete new
object or returns `PreconditionFailed` without mutation. Do not use a separate
STAT call or a pre-stream presence check as the authoritative decision.

Two simultaneous create-only PUTs for an absent key must yield one success
and one `412`. Two replace-only PUTs may both succeed in sequence while the
key remains present; wildcard conditions check existence, not object version.

For the future filesystem store, a keyed corrupt index entry counts as
present: create-only fails and replace-only can repair it. If an unkeyable
file already occupies the derived physical path, do not overwrite it based
on the absent B-tree entry; report storage corruption for local maintenance.
The filesystem implementation remains covered by its separate plan.

## 4. Local web console

Add a PUT-only selector with `Unconditional`, `Create only`, and `Replace only`
to the HTML console. Send `If-None-Match: *` or `If-Match: *` for the selected
mode. Extend the Python proxy's PUT header allowlist to forward those two
headers to the h2c server. The console's status and response body display
already handles `412`; preserve its existing file and text upload behavior.

## 5. Verification

- Exercise all three modes against absent and present keys through the store
  and HTTP/2 handler. Confirm failed conditions leave payload and content
  type unchanged, including an empty-body PUT.
- Reject duplicate, conflicting, and non-wildcard conditional headers with
  `400` before context creation. Confirm `412` has the bounded public body and
  no private store detail.
- Race two create-only PUTs for the same absent key and confirm exactly one
  succeeds. Exercise replacement alongside DELETE and confirm the result
  follows the serialized publication order.
- Verify the web console emits and the proxy forwards each selected header.
  Existing GET, HEAD, LIST, DELETE, range GET, and unconditional PUT behavior
  must remain valid.

Run focused `journey-storage` checks and tests. Do not run `cargo fmt`,
`rustfmt`, or another automated source formatter in this repository.
