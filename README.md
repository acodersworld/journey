# Journey AWS/home validation slice

This workspace contains the bounded HTTP/2-over-WebSocket transport and the
disposable real-world validation applications. The gateway owns public routes,
site Basic Authentication, and proxy backpressure. The home agent owns the
read-only media fixtures and digest-named upload storage. Application routes
and storage behavior remain outside `journey-websocket`.

The deployment bundle and teardown procedure are in
`deploy/README.md`. The implementation and acceptance gate are described in
`docs/aws-home-real-world-validation-plan.md`.

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

## In-memory HTTP/2 object storage

Run the h2c example from the workspace root:

```bash
cargo run -p journey-storage --example h2c_get_server
```

The service exposes `GET`, `HEAD`, `PUT`, and `DELETE` on `/objects/<key>`,
plus `GET /objects?prefix=<prefix>&limit=<n>&cursor=<token>` for listing. LIST
requires `prefix` (use `prefix=` for all keys); it returns lexicographically
ordered JSON pages and an unpadded Base64URL continuation token. Repeat the
same prefix when following a token. Listing is not a snapshot, so concurrent
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
