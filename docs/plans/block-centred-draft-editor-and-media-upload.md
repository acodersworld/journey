# Block-centred draft editor and media uploads

## Summary

Use one editor for new and existing drafts. Authors see an ordered set of
blocks; each block has optional heading and body text plus an ordered gallery
of zero or more files. Media child rows remain a database detail and are not
presented as "child blocks" in the editor.

## Required data shape

- Keep `post_blocks` as the one table for both visible blocks and gallery
  items. A visible block is a root row (`parent_id IS NULL`). Every gallery
  placement is a child row whose `parent_id` identifies that root block.
  Sibling `position` orders visible blocks and separately orders the media
  children within each block. Moving a gallery item between visible blocks
  changes that existing child row's parent and position; duplicating it
  creates another child row. Neither operation duplicates the original file.
- `media_assets` holds shared original-file metadata, keyed by the storage
  key. A media child row references its asset and holds text specific to that
  placement, including caption and alt text. Multiple child rows may reference
  the same asset, even within one parent block. A visible block with no media
  has no media children.
- The Rust and JSON post trees represent gallery items through
  `PostBlock.children` and `NewBlock.children`. Do **not** add a separate
  `post_media` placement table, `PostBlock.media` / `NewBlock.media` array, or
  `PostMedia` / `NewMedia` type. The asset table is for file deduplication;
  child blocks are the placements. Existing media URLs use the child block ID.

## Changes

- `/posts/new` opens the empty editor; an existing draft opens the same editor
  filled with its content. The first save or upload creates an untitled draft.
  Require a nonblank title before publishing, and keep published posts
  read-only.
- Dropping or selecting several files on a block uploads them in selection
  order and creates one media child row per gallery placement. Dragging a
  thumbnail within or between blocks **moves** that placement; a Duplicate
  control adds another placement of the same file. Each placement keeps its
  own caption and alt text. Block text describes the block as a whole.
- Maintain an internal media pool for the current editor session, with no
  visible tray. Reusing the same selected `File` shares its completed upload.
  A file selected again later may upload again; the returned content-derived
  key identifies the existing asset. Never merge files based only on name and
  size.
- Save text, gallery membership, and ordering by sending the entire ordered
  post tree in one draft update. Apply it in one database transaction with a
  revision check; do not add a separate reorder endpoint. Keep existing root
  and child IDs stable so media URLs continue to work. Update the importer
  and renderer to read and write the same child-block representation.
- Stream each file through the website to storage with backpressure and a
  configurable **2 GiB default** per-file limit. Accept JPEG, PNG, WebP, GIF,
  HEIC/HEIF, MP4, and MOV originals. Interrupted uploads retry from the
  beginning. Removing a placement detaches it without deleting the stored
  original.
- Keep Save plus autosave, visible save and upload status, and revision checks
  for concurrent edits. Serialize saves and uploads for a draft; save the
  target block before uploading to it. Allow publication with a warning that
  original media may not display in every browser, and provide an
  authenticated download fallback.

## Verification

Test an empty block, several files in one block, ordered batch drops,
reordering and moving placements, and deliberate duplicate placements. Verify
reused uploads share an asset, separately selected equal files remain valid,
and captions stay with their placements. Test first-upload draft creation,
autosave, revision conflicts, permissions, interrupted and oversized streams,
publishing warnings, and existing importer and media routes. Assert that a
multi-file gallery serializes as multiple ordered child blocks and that moving
one preserves its child ID and media URL.

## Assumptions

The media pool is internal to the open editor; persisted asset references
remain available through saved placements. Conversion, resumable upload, and
cleanup of unreferenced originals remain later work. An incompatible SQLite
database is rejected with the project's rebuild instruction rather than
migrated.
