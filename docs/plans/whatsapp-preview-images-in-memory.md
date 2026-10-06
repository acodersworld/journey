# Store WhatsApp preview-image authorizations in memory

## Summary

Replace the SQLite `share_preview_images` table with a shared memory map.
Preview-image URLs expire after **10 seconds by default**, configurable through
`site.whatsapp_preview_image_ttl_seconds`. Remove expired entries every
**60 seconds**.

## Implementation

- Share the map across cloned application state. Key entries by share-link ID
  and image-token digest; store the selected media block ID and a monotonic
  expiry deadline. Keep the existing random image URLs and token generation.
- On a valid WhatsApp preview request, create an entry whose lifetime is the
  configured TTL, capped by the share link's remaining lifetime. Repeated
  preview requests receive fresh entries.
- On each image request, reject missing or expired entries immediately,
  regardless of the cleanup schedule. Release the map lock before checking
  SQLite for link expiry, revocation, published status, and media ownership.
  Preserve current image generation and streaming behavior.
- Run cleanup once a minute through the site's existing background-task and
  shutdown lifecycle. A process restart invalidates all outstanding
  preview-image URLs.
- Add the positive TTL setting to site configuration and its example file.
  Remove the preview-image table, indexes, writes, and SQLite cleanup. Advance
  the database schema version and retain the required rebuild instruction for
  older databases.
- Update the share-link design documentation to describe the short memory
  lifetime.

## Verification

- Check the default and configured TTL, rejection of zero, access before
  expiry, rejection at expiry, and cleanup without real-time sleeps.
- Verify cloned application state shares entries, while newly created state
  after a restart has none.
- Retain coverage for invalid tokens, wrong link IDs, revoked or expired links,
  unpublished posts, and removed media.
- Confirm retries within the TTL work without extending it, and ordinary share
  sessions remain unaffected.

## Assumptions

- Only temporary image authorizations move to memory; posts, share links, and
  guest sessions remain in SQLite.
- Expiry controls admission of new requests. An image request authorized before
  expiry may finish streaming afterward.
- Cleanup remains fixed at 60 seconds; only the authorization TTL is
  configurable.
- Existing development databases must be recreated and imported again,
  following `AGENTS.md`.
