# Website post backend

**Status:** Implemented local backend and account authentication
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
the last post's `YYYY-MM-DD` publication date and ID separated by a colon. Its
optional `tag` parameter matches one stored tag exactly, including case. Both
parameters apply to the same published-post keyset query. Malformed cursors
return `400`. The database fetches one extra row to tell whether a following
page exists; cursors follow the current ordering and do not preserve a snapshot
across imports.

`GET /` renders the newest full post and embeds its cursor when older posts
exist. A small browser script uses `GET /api/posts` to discover one following
post ID at a time, then fetches its server-rendered HTML from
`GET /posts/{id}/fragment`. A visible **Load more** button supports manual
loading and retry. Tag feeds at `/tags?tag=...` use that same renderer and
pagination path, passing the selected tag through every API request. Post tags
link to their exact-case filtered feed. `/archive/{YYYY-MM}` lists published
post titles, dates, and summaries for the selected month.

Every public HTML page shares a server-rendered navigation sidebar. It lists the
five newest published posts, distinct published archive months in descending
order, and distinct published tags in case-insensitive alphabetical order
while preserving their stored spelling. The archive list starts with six
months and the tag list with twelve entries; browser controls reveal the rest.
The sidebar starts closed on each page load. At every screen width it opens as
a fixed drawer over the page, leaving the reading column and chevron in place.
A backdrop dims the page; the chevron, backdrop, and Escape close the drawer.

`GET /api/posts/{id}` remains the JSON representation, and `GET /posts/{id}`
renders a complete HTML page for direct links. All HTML surfaces use the same
Rust post renderer. Its external CSS and JavaScript are served directly by the
site without a frontend build step. Generated HTML is indented for readable
browser page source while its text nodes remain intact.

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
confirms the block belongs to an accessible post and is media before asking
storage for it. It streams response bodies and forwards single byte ranges for
video. Image ranges are ignored. There are no editing routes in this slice.

The `journey-site import` and `journey-site db` commands provide explicit
imports and read-only database inspection. `JOURNEY_SITE_DB` selects the
SQLite path, `JOURNEY_SITE_BIND` selects the website HTTP listener, and
`JOURNEY_STORAGE_H2C` selects the loopback storage listener.

## Accounts and sessions

The schema adds `users`, `sessions`, and bounded `login_throttles` tables
without changing post rows. Usernames are case-insensitive, and a partial
unique index allows at most one owner. Readers can be disabled; owners cannot.
The CLI creates accounts and changes passwords through terminal input with
echo disabled. A manifest can add accounts through its optional top-level
`users` array; an omitted role defaults to reader, and passwords are hashed
during import. Manifest password inputs remain plain text in the JSON file, so
this path is intended for local fixtures. Importing is additive: an existing
username's account and sessions are left untouched when the manifest is
imported again. There is no self-registration or user-management HTTP API.
Replacing imported posts only deletes from `posts`, so existing accounts and
sessions survive imports.

Passwords use Argon2id version 19 with 19 MiB memory, two iterations, and one
lane. Session cookies contain 256 bits of random token material, while SQLite
stores only its SHA-256 digest. Sessions have an absolute seven-day lifetime by
default, configurable with `JOURNEY_SITE_SESSION_TTL_SECONDS`. Password changes
and reader disabling delete that user's sessions. Login attempts are limited
to five per username and 30 per client IP in a 15-minute window; old rows are
pruned and the throttle table is capped at 10,000 entries.

`POST /api/auth/login`, `GET /api/auth/current`, and `POST /api/auth/logout`
are the JSON session endpoints. State-changing auth requests require an
`Origin` matching `JOURNEY_SITE_PUBLIC_ORIGIN`. Without that setting, only
loopback HTTP origins are accepted, and the server itself must bind to a
loopback address. Deployments set the public origin explicitly; HTTPS origins
cause the cookie to include `Secure`. Cookies are host-only, HttpOnly, and
SameSite=Strict. Authenticated content responses use `Cache-Control: private,
no-store`.

All existing content routes are behind one session middleware, including
`HEAD` and ranged media requests. The post access value is shared by the post
and media database lookups: readers receive published-only access and owners
can include drafts. A future post-scoped link can map to published-only access
without creating an account session or gaining draft access. Feeds, tags,
archives, and sidebar data remain published-only for every account.

The browser uses a server-rendered `GET`/`POST /login` form and `POST /logout`;
the form works without JavaScript and reuses the JSON endpoints' credential,
throttle, origin, session, and cookie rules. `GET /login`, `/site.css`, and
`/site.js` are public. Unauthenticated `GET` requests for content pages redirect
to sign-in with a `return_to` value restricted to `/`, `/tags`, `/archive/YYYY-MM`,
or `/posts/<id>` paths. Invalid targets fall back to `/`. API, fragment, and
media requests continue to return `401`. Content pages show the signed-in
username and submit logout through a native form. If feed loading receives
`401`, the browser returns to sign-in with the current content path and query.
