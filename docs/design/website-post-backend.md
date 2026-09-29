# Website post backend

**Status:** Implemented local backend slice  
**Updated:** 29 September 2026

## Runtime data

`journey-site` stores posts in SQLite. `posts` contains an auto-generated ID,
title, ISO publication date, summary, a published flag, and a JSON array of
case-preserving string tags. `post_blocks` stores blocks with an auto-generated
ID, parent ID, sibling position, optional header and body, and optional media
fields. A row with children is a group; groups can contain only one level of
children and can also have their own text and media. The content type identifies
image and video media. A migration maps legacy headings to headers, paragraphs
to bodies, and media captions to bodies; migrated posts receive empty tag
arrays. Imported posts are always published. The published flag is checked by
public post and media lookups so a later draft feature can keep unpublished
media inaccessible through these routes.

Post and block IDs are regenerated on each full import. They remain stable for
in-place edits. Feed ordering is publication date descending, then ID
descending. Blocks are ordered by zero-based position among siblings.

## Manifest import

The JSON manifest is the editable source for this local workflow. Its top
level contains `posts`; each post has `title`, `published_at` (`YYYY-MM-DD`),
`summary`, optional `tags`, and ordered `blocks`. Each block can have a plain
text `header`, plain text `body`, a media `path` with optional `alt`, and nested
`blocks`. Nested blocks are allowed only on top-level blocks. Media type is
inferred from the path extension and stored as its content type. Image and
video paths are relative to the manifest directory. The importer canonicalizes
and confines them to that directory and accepts JPEG, PNG, WebP, GIF, and MP4
extensions. It indexes media by canonical file path, so multiple blocks that
refer to the same file share one upload.

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

`journey-site serve` requires storage to accept an initial connection before
the website starts. After startup, the shared storage client reconnects to the
configured loopback address when its h2 connection ends or a request finds the
sender closed or dispatch fails. Concurrent requests share the replacement
connection. A request already in flight can fail during a disconnect; the
client does not replay uploads or restart a GET response body. Media routes
keep returning their upstream failure response while storage is unavailable,
and later requests can succeed after storage returns without restarting the
website.

`GET /api/posts` returns `{ "posts": [...], "next_cursor": string | null }`
with published summaries ordered by `published_at DESC, id DESC`. The default
page size is 10 and the maximum is 100. Its optional `after` parameter contains
the last post's `YYYY-MM-DD` publication date and ID separated by a colon. The
query uses that pair as a keyset position, including when adjacent posts share
a publication date. Malformed cursors return `400`. The database fetches one
extra row to tell whether a following page exists; cursors follow the current
ordering and do not preserve a snapshot across imports.

`GET /` renders the newest full post and embeds its cursor when older posts
exist. A small browser script uses `GET /api/posts` to discover one following
post ID at a time, then fetches its server-rendered HTML from
`GET /posts/{id}/fragment`. A visible **Load more** button supports manual
loading and retry. `GET /api/posts/{id}` remains the JSON representation, and
`GET /posts/{id}` renders a complete HTML page for direct links. All three HTML
surfaces use the same Rust post renderer. Its external CSS and JavaScript are
served directly by the site without a frontend build step. Generated HTML is
indented for readable browser page source while its text nodes remain intact.

Rows with children render as section introductions followed by their children
in sibling order; a group parent's body is its description, even when it has
media fields. Adjacent child image and video blocks form a responsive gallery,
and text children split gallery runs. Gallery items open in a keyboard and
touch navigable slideshow; standalone images use a one-item slideshow, while
standalone videos remain inline playable. Initial page images prioritize the
first image and lazy-load later images. Direct pages and fragments reuse the
same original-media URLs; image resizing, format conversion, and variants are
deferred.

HTML text and attributes are escaped. Image and video blocks use public media
URLs that identify a post and block ID, never a storage key. The backend
confirms the block belongs to a published post and is media before asking
storage for it. It streams response bodies and forwards single byte ranges for
video. Image ranges are ignored. There are no editing, draft, or account
routes in this slice.

The `journey-site import` and `journey-site db` commands provide explicit
imports and read-only database inspection. `JOURNEY_SITE_DB` selects the
SQLite path, `JOURNEY_SITE_BIND` selects the website HTTP listener, and
`JOURNEY_STORAGE_H2C` selects the loopback storage listener.
