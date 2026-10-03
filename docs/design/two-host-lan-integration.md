# Two host LAN integration

**Status:** Implemented disposable integration path
**Purpose:** Run the current site and object store on separate trusted LAN hosts
for manual browser and media validation. This is not a production deployment.

## Runtime topology

The site host runs the server-rendered `journey-site` binary and Nginx in one
Compose project. Nginx publishes HTTP on the configured site LAN address and
routes `/internal/storage` to the site's private WebSocket listener. The site
is the inner HTTP/2 client. The object host starts an outbound WebSocket
connection and serves the existing storage HTTP/2 protocol inside it.

The object host runs `journey-storage-service`, backed by `FilesystemStore`,
and exposes the existing authenticated object management UI on its LAN address.
The object directory and site SQLite database use separate persistent volumes.
Import manifests and referenced media are mounted read-only into the site
container.

The site accepts one authenticated active storage session. A configured shared
secret is checked during the WebSocket upgrade, before the inner HTTP/2 session
starts. The object service reconnects after disconnects. The site starts even
when no session is active; storage operations fail promptly until the next
session is ready. Local development retains the direct loopback h2c storage
transport.

## HTTP and cookies

`serve --allow-insecure-lan-http` permits the configured non-loopback HTTP
origin and disables the `Secure` cookie attribute for this LAN test. Share-link
creation requires the same explicit option when its public origin is non-loopback
HTTP. Without the option, existing HTTPS and loopback restrictions apply.

The integration intentionally uses HTTP and WS without encryption. The shared
secret, browser credentials, and object management credentials travel in
plaintext. Both hosts must remain on a trusted LAN with no internet port
forwarding.

## Import control

The running site creates a mode-0600 Unix control socket in a mode-0700 runtime
directory. `journey-site import --via-running-site <manifest>` sends the
container-visible path to the site. The site validates the manifest and media,
uploads media over its live storage session, then applies the destructive
published-post replacement and returns its result. Import remains an explicit
command and is never run during startup. Without the option, the CLI retains
its direct h2c import path for local development.

The disposable Compose assets and startup procedure are in
[`deploy/README.md`](../../deploy/README.md). The application and WebSocket
transport decisions are also described in
[`vertical-slice-01-http2-over-websocket.md`](vertical-slice-01-http2-over-websocket.md).
