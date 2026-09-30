# Website post backend

**Status:** Implemented local backend and authenticated website UI
**Updated:** 30 September 2026

## Runtime data

`journey-site` stores posts in SQLite. `posts` contains an auto-generated ID,
required author reference, title, nullable UTC Unix-second publication instant, summary, a
published flag, and a JSON array of case-preserving string tags. The author is
the account that created a draft or is credited by the manifest.
`post_blocks` stores blocks with an auto-generated ID, parent ID, sibling
position, optional header and body, and optional media fields. A row with
children is a group; groups can contain only one level of
children and can also have their own text and media. The content type identifies
image and video media. Imported posts are always published; HTTP-created posts
are drafts until their author or an admin publishes them. All
post and media lookups require a session: `read` accounts can access published
posts, `write` accounts can also access their own drafts, and `admin` accounts
can access every draft. Feeds and navigation lists contain published posts
only.

Post and block IDs are regenerated on each full import. They remain stable for
in-place edits. Feed ordering is publication instant descending, then ID
descending. Blocks are ordered by zero-based position among siblings.

## Manifest import

The JSON manifest is the editable source for this local workflow. Its top
level contains `posts`; each post requires an `author` username, `title`,
`published_at` (an ISO 8601 timestamp with an explicit UTC offset or `Z`),
`summary`, optional `tags`, and ordered `blocks`. The importer converts each
timestamp to UTC Unix seconds before storing it.
Each author must already exist or be added through the manifest's `users` array.
Each block can have a plain text `header`, plain text `body`, a media `path`
with optional `alt`, and nested
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

After every upload and key check succeeds, the importer adds new users, resolves
all post authors to user IDs, and replaces the entire post set in one immediate
SQLite transaction. This deletes HTTP-created drafts as well as prior imported
posts. If author resolution or the database transaction fails, the previous
post set remains available. Successfully uploaded but unreferenced objects can
remain in storage.

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
the last post's Unix-second publication instant and ID separated by a colon. Its
optional `tag` parameter matches one stored tag exactly, including case. Both
parameters apply to the same published-post keyset query. Malformed cursors
return `400`. The database fetches one extra row to tell whether a following
page exists; cursors follow the current ordering and do not preserve a snapshot
across imports.

`POST /api/posts` creates a text-only draft for a `write` or `admin` account.
The signed-in account is always the author. The request accepts a nonblank
title, optional summary and tags, and ordered blocks nested at most one level;
summary, tags, and the block list may be empty. Unknown fields reject media
references. The draft and all blocks are inserted in one transaction. The
response is `201 Created`, contains the new ID, and sets `Location` to
`/posts/{id}`. `GET /api/drafts` lists draft summaries by descending post ID;
writers see only their own, while admins see all. A read account receives
`403`. Draft summaries serialize `published_at` as `null`, and draft HTML omits
the date element.

`POST /api/posts/{id}/publish` publishes an existing draft immediately. Its
optional `published_at` integer selects a UTC Unix second; when omitted, the
server's current second is used. The route applies the origin check and allows
the draft's `write` author or an `admin`. Other posts and missing posts return
`404`, already published posts return `409`, and a future timestamp or a post
without any nonblank block header or body returns `400`. The content validation
and state transition occur in one immediate SQLite transaction. Success is
`204 No Content`.

`GET /posts/new` serves the draft form to authenticated `write` and `admin`
accounts. It accepts a title, optional summary and comma-separated tags, and
ordered text blocks with optional headers and bodies. Root blocks and one
level of child blocks can be added, removed, and reordered in the browser; the
submitted JSON preserves the visible sibling order and has no media fields.
The form posts to `POST /api/posts`, prevents repeat submissions while the
request is active, and keeps its contents visible on failure. On success it
opens the new read-only draft page with a short creation confirmation. The
author and admins can publish from that detail page. The confirmation dialog
uses server time by default or accepts a browser-local date and time override.

`GET /` renders the newest full post and embeds its cursor when older posts
exist. A small browser script uses `GET /api/posts` to discover one following
post ID at a time, then fetches its server-rendered HTML from
`GET /posts/{id}/fragment`. A visible **Load more** button supports manual
loading and retry. Tag feeds at `/tags?tag=...` use that same renderer and
pagination path, passing the selected tag through every API request. Post tags
link to their exact-case filtered feed. `/archive/{YYYY-MM}` lists published
post titles, publication times, and summaries for the selected month. Every
publication `<time>` has a UTC ISO 8601 `datetime` value and readable UTC
fallback text; the browser formats it in local time.

A JavaScript-readable `journey_timezone` cookie stores the browser's IANA time
zone with `Path=/` and `SameSite=Lax`. Invalid or absent values use UTC. On a
first visit, or after the browser's zone changes, the script writes and reads
back the cookie before reloading; blocked cookies therefore do not cause a
reload loop. The server derives archive month membership in that zone, using
the local start of a month and the local start of the next month as a
half-open UTC interval. Feed ordering remains by absolute publication instant.

Every signed-in HTML page shares a server-rendered navigation sidebar. It
lists drafts near the top for writers and admins: writers see their own, while
admins see all, ordered newest first with five initially visible. Readers do
not see a Drafts section. Each draft links to its read-only page. The sidebar
also lists the five newest published posts, distinct published archive months
in descending order, and distinct published tags in case-insensitive
alphabetical order while preserving their stored spelling. Archive months are
derived in the selected browser time zone. The archive list
starts with six months and the tag list with twelve entries; browser controls
reveal the rest. A fixed **+ New post** link appears on signed-in pages for
writers and admins except on the creation page itself; its stacking order
places it beneath the sidebar backdrop.
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

HTML text and attributes are escaped. Image and video blocks use site media
URLs that identify a post and block ID, never a storage key. The backend
confirms the block belongs to an accessible post and is media before asking
storage for it. It streams response bodies and forwards single byte ranges for
video. Image ranges are ignored. Post editing routes are not implemented.

The `journey-site import` and `journey-site db` commands provide explicit
imports and read-only database inspection. `JOURNEY_SITE_DB` selects the
SQLite path, `JOURNEY_SITE_BIND` selects the website HTTP listener, and
`JOURNEY_STORAGE_H2C` selects the loopback storage listener.

## Accounts and sessions

The schema adds `users`, `sessions`, and bounded `login_throttles` tables.
Usernames are case-insensitive. Account roles are `read`, `write`, and `admin`;
any number of admins can exist and any account can be disabled.
The CLI creates accounts and changes passwords through terminal input with
echo disabled. A manifest can add accounts through its optional top-level
`users` array; an omitted role defaults to `read`, and passwords are hashed
during import. Manifest password inputs remain plain text in the JSON file, so
this path is intended for local fixtures. Importing is additive: an existing
username's account and sessions are left untouched when the manifest is
imported again. There is no self-registration or user-management HTTP API.
Import replaces every post, including HTTP-created drafts, while existing
accounts and sessions survive imports.

Passwords use Argon2id version 19 with 19 MiB memory, two iterations, and one
lane. Session cookies contain 256 bits of random token material, while SQLite
stores only its SHA-256 digest. Sessions have an absolute seven-day lifetime by
default, configurable with `JOURNEY_SITE_SESSION_TTL_SECONDS`. Password changes
and account disabling delete that user's sessions. Login attempts are limited
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
`HEAD` and ranged media requests. Post and media lookups apply the same policy:
read accounts receive published-only access, write accounts can also read
their own drafts, and admins can read every draft. Writers can create or revoke
share links for their own published posts; admins can manage links for any
published post. Guest share links remain limited to published posts. Feeds,
tags, archives, and sidebar data remain published-only for every account.
Future edit and delete operations require the author to have `write`
permission or the account to have `admin`; author credit alone gives a `read`
account no write permission.

The development site has no schema migration process. Incompatible database
changes require recreating the SQLite database and running the destructive
importer again. Startup rejects an outdated database, including a date-text
`published_at` column, with this rebuild instruction; legacy-row migrations are
not part of the development workflow.

The browser uses a server-rendered `GET`/`POST /login` form and `POST /logout`;
the form works without JavaScript and reuses the JSON endpoints' credential,
throttle, origin, session, and cookie rules. `GET /login`, `/site.css`, and
`/site.js` are public. Unauthenticated `GET` requests for content pages redirect
to sign-in with a `return_to` value restricted to `/`, `/tags`, `/archive/YYYY-MM`,
or `/posts/<id>` paths. Invalid targets fall back to `/`. API, fragment, and
media requests continue to return `401`. Content pages show the signed-in
username and submit logout through a native form. If feed loading receives
`401`, the browser returns to sign-in with the current content path and query.
