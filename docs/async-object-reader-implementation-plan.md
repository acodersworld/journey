# Async Object Reader Implementation Plan

**Status:** Implemented for the in-memory store and HTTP/2 service; filesystem reader remains planned

**Created:** 26 September 2026  
**Scope:** Change the existing object interface, in-memory GET reader, and HTTP/2 GET delivery before implementing filesystem storage

## 1. Goal and sequencing

Make GET deliver bounded chunks through an object reader instead of requiring
`ObjectInterface::contents()` to expose the complete selected payload. Keep
the existing `ReadRange` and `ReadSpan` selection, `200` and `206` responses,
and response headers. Implement this change against the current in-memory
store and HTTP/2 service first. The filesystem store does not exist yet; its
separate plan will use the reader contract established here.

The HTTP handler owns one reusable mutable read buffer. It copies each filled
chunk into a new owned `Bytes` value for `h2::SendStream<Bytes>::send_data`.
This copy is deliberate: h2 retains ownership of sent `Bytes`, while the
reader needs its mutable buffer for the next read.

## 2. Reader interface and in-memory store

Replace `ObjectInterface::contents()` with:

```rust
fn read(
    &mut self,
    buffer: &mut bytes::BytesMut,
) -> impl Future<Output = Result<usize, StoreError>> + Send;
```

The current `ObjectInterface: Send + Sync + 'static` bound remains. The
buffer's initialized length is the maximum number of bytes requested; `read`
overwrites at most that many bytes and returns the number written. It does not
change the buffer length. A positive-length buffer returns `0` only at the end
of the selected object. A zero-length buffer returns `Ok(0)` without moving
the cursor. A successful read advances only that reader's cursor by the
returned count; an error does not advance it. Short positive reads are valid.

Keep the in-memory catalogue's `Object` immutable. Add a separate public
per-GET reader type containing a shared `Bytes` view of the selected payload
and a cursor, and use it as `StoreInterface::Object`. Full GET creates a view
of the whole payload; range GET creates a sliced view using the existing
range resolution. `ReadObject` continues to carry complete-object metadata
and the optional selected span. Add mutable access to its reader, such as
`object_mut()`. The in-memory reader copies into the supplied buffer and
advances its own cursor. Concurrent GETs share immutable bytes but never a
cursor or store lock during delivery. Keep `Object::new` and the exported
catalogue object available for constructing the in-memory store.

Update test doubles and existing tests that call `contents()` to use the new
reader contract. Test helpers may collect a reader into bytes; production GET
must use bounded reads.

## 3. HTTP/2 GET delivery

Calculate the expected response length from `ReadSpan::size()` or the full
object metadata. Preserve the current status, `Content-Type`,
`Content-Length`, `Accept-Ranges`, and `Content-Range` behavior. For a
zero-length response, send headers with `end_stream = true` and do not call
`read`.

For a nonempty response, send headers, then repeatedly:

1. Reserve and await positive h2 stream capacity.
2. Set the reusable `BytesMut` length to the minimum of that capacity, the
   remaining response length, and the existing 64 KiB segment limit.
3. Call `read` and copy the returned prefix into a fresh
   `Bytes::copy_from_slice(&buffer[..count])` for `send_data`.
4. Decrease the remaining length by `count`; set `end_stream = true` only on
   the chunk that completes the declared response length.

If `read` returns a short positive count, continue with the same rules. If
it returns `0` while bytes remain, returns more than requested, or fails,
log the key and error locally and reset the already-open h2 stream with
`INTERNAL_ERROR`. Do not send a second HTTP response after headers or claim
successful completion of a truncated body. Propagate ordinary h2 send and
capacity errors through the existing service error path.

## 4. Future filesystem reader contract

The later filesystem store creates one reader per GET with its own logical
cursor and a cloned `Arc<File>` snapshot. Its `read` uses checked
`metadata_len + selected_offset + cursor` arithmetic and Unix
`FileExt::read_at` off the async executor. To pass the mutable buffer into a
`spawn_blocking` closure, it can swap the `BytesMut` out, transfer ownership
to the task, then restore it on both success and file-read error. It advances
the cursor only after a successful read. A request future that is cancelled
may drop its returned buffer with the detached task; it must not expose a
partial cursor update to a surviving reader. No filesystem store code is
part of this change.

## 5. Verification

- Confirm full and ranged GETs preserve status, headers, exact bodies, and
  zero-length behavior, including small h2 capacity grants and multiple
  chunks.
- Confirm independent readers over the same object start at their own cursor,
  short reads advance by the returned count, and a zero-length buffer leaves
  the cursor unchanged.
- Use a test reader that returns an early EOF or a read error after headers;
  confirm the stream resets and no truncated body is reported as complete.
- Run focused `journey-storage` checks and tests. The implementation passed
  `cargo test -p journey-storage` (74 tests) on 26 September 2026. Do not run
  `cargo fmt`, `rustfmt`, or another automated source formatter in this
  repository.
