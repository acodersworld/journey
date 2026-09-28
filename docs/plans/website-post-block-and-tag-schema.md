# Post block and tag schema

## Summary

Focus this change on SQLite and the post data model. The current frontend is
temporary: make only the changes needed for it to read and display the new
data. Do not build gallery or slideshow UI as part of this work.

## Database changes

- Give each block an `id INTEGER PRIMARY KEY AUTOINCREMENT`, optional
  `parent_id`, and sibling-scoped `position`. A row with children is a group.
  Groups are limited to one level; a group row may have its own media, header,
  and body.
- Replace `kind`, `text`, `level`, and `caption` with optional plain-text
  `header` and `body`. Keep optional media fields. Determine image or video
  from `content_type`.
- Add `tags` to the main `posts` row as a JSON array of strings, defaulting to
  `[]`. Use “tags” consistently in the database and Rust types. This needs no
  separate tag table or tag-filtering feature yet.
- Migrate existing flat blocks into top-level rows, mapping headings to
  `header`, paragraphs to `body`, and media captions to `body`. Existing posts
  receive empty tag arrays.

## Data paths

- Update database reads and writes, the manifest importer, and the example
  manifest for one-level groups and post tags. Preserve the importer's current
  full-replacement behavior.
- Return nested blocks and tags from post reads. Change media lookups to use
  block IDs while retaining the published-post access check.
- Adapt the existing HTML and browser rendering only enough to handle the new
  response shape. The SQL structure must not encode a gallery or slideshow
  layout.

## Verification

Test migration, tag storage and retrieval, sibling ordering, mixed text and
media children, a group row with its own media, one-level enforcement, and
unpublished media access.

## Assumptions

Tags are post-level, case-preserving strings. Block IDs remain stable during
in-place edits; the temporary full importer regenerates them.
