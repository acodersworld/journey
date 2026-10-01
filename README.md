# Journey prototypes

This workspace contains the bounded HTTP/2-over-WebSocket transport, the
disposable gateway/home validation applications, and a separate local
`journey-site` website and post backend. The validation gateway owns its
prototype routes, Basic Authentication, and proxy backpressure. The home agent
owns the read-only media fixtures and digest-named upload storage. Application
routes and storage behavior remain outside `journey-websocket`.

The deployment bundle and teardown procedure are in `deploy/README.md`.
Use `scripts/aws-home-real-world-validation.sh` with the browser and lifecycle
checklist in `docs/aws-home-real-world-validation-checklist.md` to repeat the
validation.

## Local Compose

Create an uncommitted `.env.validation` with the four credentials, create
fixtures outside Git, and prepare the upload directory:

```bash
cp deploy/gateway.env.example .env.validation
chmod 600 .env.validation
mkdir -p deploy/fixtures deploy/uploads
scripts/create-validation-fixtures.sh deploy/fixtures
docker compose up --build
```

The local gateway is published on port 8080 and uses plain WebSocket only for
local development. The AWS bundle uses Nginx, TLS, HTTP/2, and WSS instead.
Use credentials from the environment file when calling `/health` or the media
routes.

## Website post backend and UI

The `journey-site` application imports a JSON manifest into SQLite and serves
post and media routes to authenticated accounts. Start the storage example
with both listeners on loopback and persistent storage:

```bash
JOURNEY_STORAGE_BIND=127.0.0.1:8081 \
JOURNEY_STORAGE_WEB_BIND=127.0.0.1:8082 \
cargo run -p journey-storage --example h2c_get_server -- --storage-dir ./journey-storage-data
```

Create a manifest next to its local media files, for example:

```json
{
  "users": [
    { "username": "alice", "password": "local-example-password", "role": "write" }
  ],
  "posts": [
    {
      "author": "alice",
      "title": "A first journey",
      "published_at": "2026-09-27T12:00:00Z",
      "summary": "Notes from the road.",
      "tags": ["coast", "weekend"],
      "blocks": [
        {
          "header": "Along the coast",
          "body": "We set out before sunrise.",
          "path": "media/coast.jpg",
          "alt": "Coast at dawn",
          "blocks": [
            { "body": "The first light." },
            { "path": "media/harbour.mp4", "body": "A quiet harbour." }
          ]
        }
      ]
    }
  ]
}
```

Import and run the website backend in another terminal:

```bash
cargo run -p journey-site -- import ./posts.json
cargo run -p journey-site -- db posts
cargo run -p journey-site -- users list
cargo run -p journey-site -- serve
```

Open `http://127.0.0.1:8080/` and sign in with the imported `alice` account.
The server-rendered sign-in form works without JavaScript. After sign-in, the
browser returns to the requested post or feed page. Signed-in pages show the
username and a Sign out button at the top right.

Account creation and password changes prompt for a password twice without
echoing it. Passwords are not accepted as command-line arguments. Create
accounts with `users create <username> <read|write|admin>`. The CLI also
supports `users password <username>`, `users disable <username>`, and
`users enable <username>`; changing a password or disabling an account revokes
that account's active sessions. Any number of admin accounts can exist.

An import manifest may also contain a top-level `users` array with
`username`, `password`, and optional `role` fields. Roles are `read`, `write`,
and `admin`; imported accounts default to `read` and are added only when the
username is new. Every manifest post requires an `author` username that names
an existing or newly imported account. Re-importing does not replace an
existing account's password or role, or revoke its sessions. The sample
`apps/site/example/posts.json` includes `user` / `pass` (`write`), `user2` /
`pass2` (`read`), and an `admin` account; sample passwords are stored as plain
text in that manifest. Each import replaces the entire post set, including
HTTP-created drafts, while existing accounts and sessions remain.

`JOURNEY_SITE_DB` selects the SQLite file, `JOURNEY_SITE_BIND` selects the
HTTP listener (default `127.0.0.1:8080`), and `JOURNEY_STORAGE_H2C` selects the
loopback storage address (default `127.0.0.1:8081`). Set
`JOURNEY_SITE_PUBLIC_ORIGIN` to the exact public origin when deploying, for
example `https://journal.example.com`; it is required when binding outside
loopback. Local loopback HTTP origins are accepted when this variable is unset.
The `journey_session` cookie is host-only, HttpOnly, and SameSite=Strict. It is
marked Secure when the configured public origin uses HTTPS. Sessions expire
after seven days by default; set `JOURNEY_SITE_SESSION_TTL_SECONDS` to change
the absolute lifetime. The media upload limit defaults to 2 GiB per file and
can be changed with `JOURNEY_SITE_MAX_MEDIA_UPLOAD_BYTES`.

The JSON authentication endpoints are `POST /api/auth/login`,
`GET /api/auth/current`, and `POST /api/auth/logout`. Login and logout require
an `Origin` matching the configured public origin (or a loopback HTTP origin
for local development). Login accepts a JSON object with `username` and
`password`, returns the current account JSON, and sets the session cookie.
`GET /api/auth/current` returns that account, and logout returns `204` after
revoking the cookie's session. The browser also has `GET` and `POST /login` and
`POST /logout` form routes. All content and media require the session cookie.
Unauthenticated HTML page requests redirect to `/login` with a validated local
return path. Protected APIs, post fragments, and media requests return `401`.
The login page, CSS, and JavaScript load without a session. `read` accounts see
published content. `write` accounts also see drafts they authored by direct
post, fragment, API, and media URLs; `admin` accounts can see every draft.
Feeds, tags, archives, and sidebar lists include published posts only. There
is no anonymous access to posts or their media.

The authenticated feed is server-rendered at `GET /`; its browser script loads
additional full posts near the end of the page. It uses `GET /api/posts` to find
the next post and `GET /posts/{id}/fragment` to append its HTML. The API
defaults to 10 summaries and accepts a maximum `limit` of 100. Feed responses
include `posts` and a `next_cursor`; pass that cursor as `after` to request the
next page. `GET /api/posts/{id}` returns tags and nested full content as JSON,
and the normal link `GET /posts/{id}` renders it as HTML. Draft pages use the
same block editor as `/posts/new`; published posts stay read-only. The editor
autosaves text and ordered galleries, supports moving and duplicating media
placements, and keeps each placement's caption and alt text. Saving the first
time or uploading a file creates an untitled draft. A nonblank title is
required before publishing. The collapsible sidebar links to recent posts,
monthly archives, and tag-filtered feeds. Media is served only through `GET`
or `HEAD /posts/{id}/blocks/{block_id}/media`; adding `?download=1` downloads
the original through the authenticated site route. Video requests forward one
byte range to storage.

`POST /api/posts` creates a draft for a signed-in `write` or `admin` account.
Its title may be blank while drafting; summary, tags, and blocks may also be
empty. The response is `201 Created` with the draft JSON and a
`Location: /posts/{id}` header. `PUT /api/posts/{id}` replaces the entire
ordered block tree with a revision check, keeping existing block IDs stable.
`POST /posts/{id}/blocks/{block_id}/media` streams one original file through
the site to storage and returns its content-derived asset key. The default
per-file limit is 2 GiB; set `JOURNEY_SITE_MAX_MEDIA_UPLOAD_BYTES` to change
it. JPEG, PNG, WebP, GIF, HEIC, HEIF, MP4, and MOV are accepted. Each gallery
placement is a media child row and references shared `media_assets` metadata;
duplicating a placement never duplicates the stored file. `GET /api/drafts`
lists a write account's drafts by newest ID first; admins receive all drafts.

The author or an admin can publish from the draft editor. A title and at least
one nonblank block header or body are required. The publish dialog warns that
original media may not display in every browser and offers an optional past
local date and time. `POST /api/posts/{id}/publish` accepts an optional UTC
Unix-second `published_at`; without it, the server's current second is used.
Publication times are returned as integer seconds. The browser formats them
in local time, and archive month membership follows the browser's IANA time
zone.

This development site has no schema migration process. After an incompatible
site database change, recreate the SQLite database and run the destructive
importer again. An outdated database is rejected with that rebuild instruction.

`journey-site --help`, `journey-site db`, and `journey-site users --help` list
the CLI commands.

## HTTP/2 object storage

Run the h2c example from the workspace root. With no arguments, it uses an
in-memory store seeded with the included image and video:

```bash
cargo run -p journey-storage --example h2c_get_server
```

The example serves the h2c API at `http://127.0.0.1:8081` and starts a
Basic-authenticated browser object manager at `http://127.0.0.1:8082`.
The example manager credentials are `user` / `pass`. The web listener
defaults to `0.0.0.0:8082` and uses plain HTTP; set
`JOURNEY_STORAGE_WEB_BIND` to change it. The h2c listener keeps its separate
`JOURNEY_STORAGE_BIND` setting.

To use persistent filesystem storage instead, specify a directory. The server
creates it if needed and loads its existing objects on startup; it does not
seed the example image and video in this mode:

```bash
cargo run -p journey-storage --example h2c_get_server -- --storage-dir ./storage-data
```

The service exposes `GET`, `HEAD`, `PUT`, and `DELETE` on `/objects/<key>`,
plus `GET /objects?prefix=<prefix>&limit=<n>&cursor=<token>` for listing. LIST
requires `prefix` (use `prefix=` for all keys); it returns lexicographically
ordered JSON pages and an unpadded Base64URL continuation token. The token
marks the inclusive next key position and can be combined with any prefix;
prefix filtering remains independent. Listing is not a snapshot, so concurrent
inserts and deletes can affect later pages.

In another terminal, upload the included image, download it, and compare the
bytes:

```bash
curl --http2-prior-knowledge \
  -X PUT \
  -H 'Content-Type: image/jpeg' \
  --data-binary @crates/journey-storage/examples/assets/image.jpg \
  http://127.0.0.1:8081/objects/uploaded.jpg

curl --http2-prior-knowledge \
  http://127.0.0.1:8081/objects/uploaded.jpg \
  --output downloaded.jpg

cmp crates/journey-storage/examples/assets/image.jpg downloaded.jpg
```

Request and save the first 100 bytes of `image.jpg`:

```bash
curl --http2-prior-knowledge \
  -H 'Range: bytes=0-99' \
  -D - \
  http://127.0.0.1:8081/objects/image.jpg \
  --output first-100-bytes.bin
```

The response is `206 Partial Content` with `Content-Range: bytes 0-99/1222`
and a 100-byte payload.

`journey-storage` also exports a filesystem-backed `StoreInterface`
implementation. Create it asynchronously and pass it to the same generic HTTP
service:

```rust,ignore
let store = FilesystemStore::open(FilesystemStoreConfig::new(
    "/var/lib/journey/object-store",
)).await?;
let service = Service::new(store);
```

The filesystem store uses `objects/` and `part/` beneath that root, rebuilds
its ordered index at startup, and defaults its abnormal-event journal to
`journal` there. See [the deployment log rotation example](deploy/README.md#filesystem-object-store-journal).

Inspect metadata and list the catalogue:

```bash
curl --http2-prior-knowledge --head \
  http://127.0.0.1:8081/objects/uploaded.jpg

curl --http2-prior-knowledge --get \
  --data-urlencode 'prefix=' \
  --data-urlencode 'limit=100' \
  http://127.0.0.1:8081/objects
```

Delete is idempotent and returns `204 No Content`, including when the key is
already absent:

```bash
curl --http2-prior-knowledge \
  -X DELETE \
  http://127.0.0.1:8081/objects/uploaded.jpg
```

Replace the same key with the included video and inspect the updated content
type in the GET response headers:

```bash
curl --http2-prior-knowledge \
  -X PUT \
  -H 'Content-Type: video/mp4' \
  --data-binary @crates/journey-storage/examples/assets/video.mp4 \
  http://127.0.0.1:8081/objects/uploaded.jpg

curl --http2-prior-knowledge \
  -D - \
  http://127.0.0.1:8081/objects/uploaded.jpg \
  --output downloaded.mp4

cmp crates/journey-storage/examples/assets/video.mp4 downloaded.mp4
```

The current implementation retains complete uploads and stored objects in
process memory without size limits. Restrict access and upload sizes until
explicit bounds are in place; untrusted uploads can exhaust available memory.

## Checks

Do not run `cargo fmt`, `cargo fmt --check`, `rustfmt`, or another automated
source formatter in this repository. Targeted checks are:

```bash
cargo check --workspace --locked
cargo test --workspace --locked
bash -n scripts/*.sh
```

The AWS driver is `scripts/aws-home-real-world-validation.sh`. It requires
protected environment/configuration supplied by the operator and never uses
`curl -k` or disables TLS verification.
