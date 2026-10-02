# Two-host LAN integration test

## Summary

Run the object service on one LAN host and Nginx plus `journey-site` on another,
using separate Docker Compose projects. Browsers use HTTP. The object service
connects outbound over WS and carries the existing HTTP/2 storage protocol
inside that connection. This is a disposable integration test, not a
production deployment.

## Application changes

- Add a deployable object-service binary using `FilesystemStore` and the
  existing management UI. It connects to
  `ws://<site-host>/internal/storage`, reconnects after disconnection, and
  passes inner HTTP/2 requests to the existing storage request handler.
  WebSocket handling remains outside the storage crate.
- Add a WebSocket listener inside `journey-site`. Nginx routes
  `/internal/storage` to that listener and browser requests to the site's HTTP
  listener. Authenticate the WebSocket handshake with a configured shared
  secret, accept one active object-service session, and let storage operations
  recover after the session reconnects. Keep direct loopback h2c mode for
  local development. The site must start and serve pages while storage is
  unavailable; storage operations should fail promptly until it reconnects.
- Add an explicit `--allow-insecure-lan-http` opt-in for this test. It permits
  a non-loopback HTTP public origin and disables the cookie `Secure` attribute.
  Apply the same opt-in to CLI share-link creation. Keep the current HTTPS
  restrictions by default.
- Add a private Unix control socket to the running site. The command
  `journey-site import --via-running-site <manifest>` sends a container-visible
  manifest path to the site, which validates and applies the import using its
  live storage session. Return the import result or error to the CLI. Keep the
  existing direct h2c import command for local development. Imports remain
  explicit and destructive; never run one automatically at startup.

## Deployment assets

- Remove the tracked prototype AWS/home Compose files, their old environment
  files and examples, and TLS-specific deployment instructions. Replace the
  prototype Dockerfile, Nginx configuration, and deployment README with assets
  for the site and object-service binaries. Name the new Compose files
  `site-compose.yml` and `storage-compose.yml`. Leave untracked local media
  and upload data alone.
- Persist the SQLite database on the site host and the object directory on the
  storage host. Mount import manifests and referenced media read-only into the
  site container. Publish only Nginx and the object management UI on their
  respective LAN addresses; keep both site listeners private to the Compose
  network.
- Configure Nginx for WebSocket upgrades, long-lived storage connections, and
  streamed large uploads. Set the inner HTTP/2 flow-control windows high
  enough for media transfers. Document LAN addresses, the shared secret,
  startup commands, and the explicit import command.

## Verification

- Validate both Compose configurations and the Nginx configuration.
- Test browser login and share-link creation over HTTP, including the expected
  cookie behavior under the explicit LAN opt-in.
- Test manifest import through the control socket, large media upload and video
  playback, byte-range requests, and the object management UI.
- Test rejected WebSocket credentials, startup with the object host offline,
  and recovery after restarting either host. Retain coverage of direct h2c
  development mode.

## Assumptions

Both hosts are on a trusted home LAN with no internet port forwarding. HTTP,
WS, and the shared secret travel unencrypted for this disposable test.
