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
