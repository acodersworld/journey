# Two host LAN integration

This bundle runs `journey-site` behind Nginx on one LAN host and the filesystem
object service on another. Browser traffic uses HTTP. The storage host opens an
outbound `ws://` connection to the site host; HTTP/2 storage requests travel
inside that WebSocket session. This is a disposable test for a trusted LAN.
Do not forward either port from the internet.

## Build and copy the images

Build separate site and storage images from the current commit. The helper uses
one temporary depth-one clone, so staged, unstaged, and untracked changes in
the checkout are excluded:

    ./deploy/build-image.sh all
    docker image save journal-site:latest -o journey-site.tar
    docker image save storage:latest -o journey-storage.tar
    scp journey-site.tar <site-host>:/tmp/
    scp journey-storage.tar <storage-host>:/tmp/

On the site host, load the site image and copy the `deploy/` bundle:

    docker image load -i /tmp/journey-site.tar

On the storage host, load the storage image and copy the `deploy/` bundle:

    docker image load -i /tmp/journey-storage.tar

The image tags must match `JOURNEY_SITE_IMAGE` and `JOURNEY_STORAGE_IMAGE` in
their respective host environment files. For remote hosts, transfer the image
archives and deployment files by your usual trusted LAN or SSH method.

## Configure the shared secret

Generate a long secret once, for example with `openssl rand -hex 32`. Put the
same value in `site.toml` and `storage.toml` as `websocket_secret`. The secret
and all HTTP, WebSocket, browser, and management traffic are unencrypted in
this test. Keep the hosts on a trusted LAN and do not configure internet port
forwarding.

## Start the site host

Copy `site-compose.yml`, `nginx.conf`, `site.env.example`, and
`site.toml.example` into a deployment directory on the site host. Copy the
examples to `.env` and `site.toml`, then set:

- `SITE_LAN_IP` to the site's LAN address.
- `JOURNEY_SITE_IMAGE` to the site image tag loaded above.
- `JOURNEY_IMPORT_DIR` to a host directory containing manifests and their
  referenced media files.
- `site.toml`'s `site.public_origin` to `http://` followed by the site's LAN
  address, and `storage.websocket_secret` to the shared secret.

The container runs as UID 65532. Make the config readable only by that service
account, then start the site:

    chmod 600 .env
    sudo chown 65532:65532 site.toml
    sudo chmod 400 site.toml
    docker compose -f site-compose.yml up -d
    docker compose -f site-compose.yml ps

Only Nginx is published, bound to `SITE_LAN_IP:80`. Both site listeners stay
private to the Compose network. The SQLite database is stored in the persistent
`site-data` volume. The import directory is mounted read-only at `/imports`.

The site can start while the storage host is offline. Pages and authentication
remain available; media operations return an error until the storage host
reconnects. Nginx keeps the storage WebSocket open for an hour between traffic
and streams large upload and download bodies without buffering them to disk.
Inner HTTP/2 stream and connection windows are configured to 32 MiB and 64 MiB
on both ends of the session.

## Start the storage host

Copy `storage-compose.yml`, `storage.env.example`, and
`storage.toml.example` to the storage host. Copy the examples to `.env` and
`storage.toml`, then set:

- `STORAGE_LAN_IP` to the storage host's LAN address.
- `JOURNEY_STORAGE_IMAGE` to the storage image tag loaded above.
- `storage.toml`'s `site_connection.websocket_url` to
  `ws://<site-LAN-address>/internal/storage`.
- `storage.toml`'s `site_connection.websocket_secret` to the same secret as the
  site host, and `management.username` / `management.password` to unique
  credentials.

The container runs as UID 65532. Make the config readable only by that service
account, then start the object service:

    chmod 600 .env
    sudo chown 65532:65532 storage.toml
    sudo chmod 400 storage.toml
    docker compose -f storage-compose.yml up -d
    docker compose -f storage-compose.yml logs -f storage

Only the object management UI is published, on `STORAGE_LAN_IP:8082`. The
SQLite-backed site data remains on the site host; filesystem objects persist in
the storage host's `object-data` volume. The storage WebSocket client retries
after connection failures and restarts.

## Import and use the site

Place the manifest and all media paths it references under the configured
`JOURNEY_IMPORT_DIR`. The site container sees that directory under `/imports`.
Run imports explicitly through the live storage session:

    docker compose -f site-compose.yml exec site \
      journey-site import --via-running-site /imports/posts.json

Imports replace the published post set and can add imported users. They never
run automatically at startup. The direct h2c importer remains available for
local development without `--via-running-site`.

Open `http://<site-LAN-address>` in a browser to log in and exercise share links,
media uploads, video seeking, and byte-range requests. To create a LAN share
link from the running site container, use:

    docker compose -f site-compose.yml exec site \
      journey-site share-links create <published-post-id> --allow-insecure-lan-http

Open `http://<storage-LAN-address>:8082` for the authenticated object
management UI.

## LAN checks

Validate the Compose files on each host before starting the services:

    docker compose -f site-compose.yml config --quiet
    docker compose -f storage-compose.yml config --quiet

On the site host, validate Nginx with the mounted configuration:

    docker compose -f site-compose.yml run --rm --no-deps \
      nginx nginx -t

Check the site login and share-link flow in a browser over HTTP. In the browser
developer tools, confirm the `journey_session` cookie has `HttpOnly` and
`SameSite=Strict` and does not have `Secure` under the LAN opt-in. A generated
share link should start with `http://<site-LAN-address>/share/`.

Check that the storage endpoint rejects an incorrect secret with HTTP 401:

    curl --include --no-buffer \
      -H 'Connection: Upgrade' \
      -H 'Upgrade: websocket' \
      -H 'Sec-WebSocket-Version: 13' \
      -H 'Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==' \
      -H 'Sec-WebSocket-Protocol: h2-over-websocket-v1' \
      -H 'X-Journey-Storage-Secret: incorrect' \
      http://<site-LAN-address>/internal/storage

Stop the storage Compose project and confirm the site still serves its login
page while media requests fail promptly. Start the storage project again and
confirm its logs show a new storage session. Restart the site project and then
the storage project to check both reconnect directions. In the browser, upload
a large media file, play and seek within an imported video, and confirm the
browser's video range requests return partial content. Check the object UI at
the storage LAN address and verify a stored object can be listed and
downloaded.

If the site reports that its database schema is outdated, stop the site,
recreate the `site-data` volume, restart it, and run the destructive importer
again. This development deployment has no database migration process.
