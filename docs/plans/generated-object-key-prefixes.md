# Prefixes for generated object keys

**Status:** Ready for implementation
**Created:** 28 September 2026

## Summary

Let generated-key uploads choose a logical prefix. `PUT /objects/photos/`
with `Object-Key-Mode: sha256` publishes the object as `photos/<digest>`;
`PUT /objects` remains the root upload. Reserve trailing `/` for prefixes, so
stored object keys cannot end in `/`.

## Interface changes

- Extend the generated-key mode to carry an optional prefix. Decode it once
  using the existing object-path rules, require a nonempty prefixed value to
  end in `/`, and require `prefix length + 64` to fit the 1,024-byte key limit.
  The root prefix is empty.
- On a prefixed generated PUT, return the **full** key in `Object-Key`. Keep
  the existing content type requirement, conditional PUT behavior, status
  codes, and streaming publication.
- Reject caller-keyed PUTs, GETs, HEADs, and DELETEs whose logical key ends in
  `/`. A prefixed PUT without `Object-Key-Mode: sha256`, or a mode header on a
  path without trailing `/`, returns `400`. Keep `PUT /objects/` invalid.
- Extend `PutKey::Sha256` to include the prefix and apply the trailing-slash
  rule in `Key::new`. Update both backends to derive `prefix + lowercase
  SHA-256 hex` before the publication condition check. Prefixes remain
  virtual; they do not change the filesystem layout.

## Existing data and verification

- At startup, leave existing files whose embedded keys end in `/` untouched,
  skip them from the index, and report them through the existing
  abnormal-event journal and stderr. No migration or automatic deletion is
  included.
- Test root and prefixed uploads, full returned keys, the length boundary,
  invalid route and header combinations, and create-only races within one
  prefix. Confirm the same bytes under different prefixes produce separate
  objects.
- Update web listing tests and design docs for the new rule that folder
  prefixes cannot also be object keys.

## Assumptions

- Apart from the trailing `/` rule, this patch does not add path
  normalization or new key-character restrictions.
- The SHA-256 digest remains the final 64 characters of a generated key.
