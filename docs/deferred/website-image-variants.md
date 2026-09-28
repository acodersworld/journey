# Deferred website image variants

**Status:** Deferred  
**Recorded:** 28 September 2026

The website currently stores a media storage key and content type directly on
each post block and serves the original file. Responsive image generation and
delivery are later work; no image processing is part of the current backend.

## Proposed database shape

- Each media-bearing post block references one `media_assets` row instead of
  storing its own storage key. The asset records the original file's storage
  key and content type. Multiple blocks may reference the same asset.
- An `image_variants` table holds zero or more generated files for each image
  asset. Each variant records its pixel width and height, content type, and
  storage key. The original remains available even when no variants exist.
- Keep the block's alt text, header, and body on the block: they describe that
  use of the media, not the shared file. Video assets need no image variants.

Do not store variant references as a JSON array on each block. Separate rows
allow multiple sizes and formats, shared assets, and direct lookup of the
available files.

When this work is scheduled, decide the generated sizes and formats, how and
when variants are made and removed, the responsive media URL and `srcset`
contract, cache policy, and migration from block-level storage keys. No such
decisions or implementation are required now.
