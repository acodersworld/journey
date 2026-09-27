# Website post backend

**Status:** Implemented local backend slice  
**Updated:** 28 September 2026

## Runtime data

`journey-site` stores posts in SQLite. `posts` contains an auto-generated ID,
title, ISO publication date, summary, and a published flag. `post_blocks`
contains the ordered paragraph, heading, image, and video blocks, with a
foreign key back to each post. Imported posts are always published. The
published flag is checked by public post and media lookups so a later draft
feature can keep unpublished media inaccessible through these routes.

Post IDs are regenerated on each full import. Feed ordering is publication
date descending, then ID descending. Blocks are ordered by their zero-based
position.

## Manifest import

The JSON manifest is the editable source for this local workflow. Its top
level contains `posts`; each post has `title`, `published_at` (`YYYY-MM-DD`),
`summary`, and ordered `blocks`. Blocks use `type` values `paragraph`,
`heading`, `image`, and `video`. Image and video paths are relative to the
manifest directory. The importer canonicalizes and confines them to that
directory and accepts JPEG, PNG, WebP, GIF, and MP4 extensions. It indexes
media by canonical file path, so multiple blocks that refer to the same file
share one upload.

The importer validates the manifest and every referenced file before upload.
For each unique path, it streams the file once to `PUT /objects/media/` with
`Object-Key-Mode: sha256`, its content type, and its measured content length.
Storage computes the key and returns the generated digest in one `Object-Name`
header. The site accepts only `200 OK` responses with exactly one valid
64-character lowercase hexadecimal name, then combines it with `media/` to
form the full storage key. Uploads are unconditional, so repeated imports
receive the same key for the same contents. Different paths with identical
contents are uploaded separately and resolve to the same key.

After every upload and key check succeeds, the importer resolves media blocks
to their returned keys and replaces the post set in one immediate SQLite
transaction. If validation, upload, key checking, or the database transaction
fails, the previous post set remains available. Successfully uploaded but
unreferenced objects can remain in storage.

## Storage and HTTP boundary

The website uses the application-level `StorageClient` interface. Its local
implementation speaks h2c to the existing storage example, and enforces a
loopback address. This keeps storage protocol details out of the website
handlers and leaves room for a later WebSocket-backed implementation without
changing `journey-storage`.

`GET /api/posts` returns at most 100 published post summaries, newest first.
`GET /api/posts/{id}` and `GET /posts/{id}` return one post with blocks. Public
media URLs identify a post and zero-based block position, never a storage key.
The backend confirms the block belongs to a published post and is an image or
video before asking storage for it. It streams response bodies and forwards
single byte ranges for video. Image ranges are ignored. There are no editing,
draft, or account routes in this slice.

The `journey-site import` and `journey-site db` commands provide explicit
imports and read-only database inspection. `JOURNEY_SITE_DB` selects the
SQLite path, `JOURNEY_SITE_BIND` selects the website HTTP listener, and
`JOURNEY_STORAGE_H2C` selects the loopback storage listener.
