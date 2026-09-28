# Storage-generated SHA-256 object keys

**Status:** Ready for implementation
**Created:** 27 September 2026

## Summary

Add a streaming upload that lets the storage service derive the logical key
from the payload. Keep caller-keyed uploads and the existing GET, HEAD, LIST,
and DELETE routes. This change covers the storage crate and its example server
only; site integration is a later patch.

## Interface and behavior

- Keep `PUT /objects/<key>` for caller-supplied keys. Add `PUT /objects` for
  generated keys, requiring `Object-Key-Mode: sha256`. A bare `PUT /objects`,
  duplicate mode headers, unsupported modes, and queries on this route return
  `400`.
- Both PUT routes require `Content-Type` and use the existing conditions:
  unconditional by default, `If-None-Match: *` for create-only, and
  `If-Match: *` for replace-only. Evaluate the condition against the **derived
  key at publication time**.
- Derive the key as the 64-character lowercase hexadecimal SHA-256 digest of
  the payload bytes, including for an empty payload. On success, both PUT
  routes return `200`, `Object-Key: <key>`, and an empty body. Generated uploads
  return no `Location` header.
- Extend `StoreInterface` so a PUT context can hold either a supplied key or a
  SHA-256-generated key, and publication returns the final `Key`. Update both
  the filesystem and in-memory backends. The filesystem backend uses its
  existing incremental upload digest and part-file publication path; it does
  not publish under a temporary logical key or rename a published object.
- With identical bytes and a different `Content-Type`, unconditional or
  replace-only publication updates the object metadata, just as keyed PUT
  does. Create-only returns `412` if the digest key already exists.

## Verification

- Exercise generated uploads through HTTP/2 against both backends: returned
  key, GET/HEAD/LIST by that key, empty payload, and unchanged keyed PUT
  behavior.
- Check all three publication conditions, including concurrent create-only
  uploads of identical bytes.
- Confirm invalid mode headers and bare `PUT /objects` fail without
  publishing, and cancelled or failed streams leave no published object.
- Document the route, headers, and examples in the current storage design.

## Assumptions

- The generated key is the digest alone, with no namespace prefix.
- No site client, web console, or WebSocket code changes are included.
