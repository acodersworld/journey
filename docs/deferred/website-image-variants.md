# Deferred website image variants

**Status:** Deferred  
**Recorded:** 28 September 2026

The website stores shared original-file metadata in `media_assets`. Each
gallery placement is a media child row in `post_blocks` and points to that
asset; its caption and alt text stay on the placement. The website serves the
original file. Responsive image generation and delivery are later work; no
image processing is part of the current backend.

## Proposed database shape

- An `image_variants` table holds zero or more generated files for each image
  asset. Each variant records its pixel width and height, content type, and
  storage key. The original remains available even when no variants exist.
- Keep each placement's alt text, label, and caption on its child row: they
  describe that use of the media, not the shared file. Video assets need no
  image variants.

Do not store variant references as a JSON array on each block. Separate rows
allow multiple sizes and formats, shared assets, and direct lookup of the
available files.

When this work is scheduled, decide the generated sizes and formats, how and
when variants are made and removed, the responsive media URL and `srcset`
contract, and cache policy. No such decisions or implementation are required
now.
