# Disposable AWS/home validation deployment

This directory is the small deployment bundle described by
docs/aws-home-real-world-validation-plan.md. It intentionally contains no
certificates, fixtures, uploads, or passwords.

## Build and deliver the image

Run these commands on an x86 development machine from the repository root:

    git_sha=$(git rev-parse HEAD)
    docker build --platform linux/amd64 -t "journey-real-test:$git_sha" .
    docker context create journey-aws --docker "host=ssh://ubuntu@<aws-host>"
    docker image save "journey-real-test:$git_sha" \
      | docker --context journey-aws image load

Copy aws-compose.yml, nginx.conf, the tls/ directory containing only the leaf
certificate and key, and a restrictive gateway.env to /opt/journey-test. Set
JOURNEY_IMAGE=journey-real-test:<git-sha> in the remote shell environment and
run:

    cd /opt/journey-test
    docker compose -f aws-compose.yml up -d
    docker compose -f aws-compose.yml ps

The AWS security group publishes only TCP 443. Port 9000 is private to the
Compose network and port 8080 is not published.

## Home agent

Copy home-compose.yml, home.env, and the CA certificate (not the CA private
key) to the home host. Set JOURNEY_IMAGE, JOURNEY_TEST_DOMAIN,
HOME_FIXTURES_DIR, and HOME_UPLOADS_DIR in the shell environment. The fixture
directory is mounted read-only and the upload directory is mounted writable.
The upload directory must be writable by UID/GID 65532.

The home environment file contains only the home credential:

    cp deploy/home.env.example home.env
    chmod 600 home.env

Start it with:

    docker compose -f home-compose.yml up -d
    docker compose -f home-compose.yml logs -f home

## Filesystem object-store journal

When the filesystem store is configured, its JSON Lines event journal defaults
to `journal` under the storage root. Set a different journal path with
`FilesystemStoreConfig::with_journal_path`. Install a host `logrotate` stanza
for that path and adjust its size and retention values as needed; the defaults
below rotate at 10 MiB and keep five numbered files:

    /var/lib/journey/storage/journal {
        size 10M
        rotate 5
        missingok
        notifempty
    }

Use rename based rotation and omit `copytruncate`. The store opens the current
journal path for every event, so later events append to the newly created
`journal` after rotation. Logrotate keeps numbered files such as `journal.1`
and `journal.2`.

## Private test CA

Generate the CA and domain certificate on the development machine. Keep the CA
private key there; only copy test-ca.crt to home and the leaf certificate and
key to AWS. Replace validation.example.test with the exact test-domain name:

    mkdir -p deploy/tls
    openssl genrsa -out deploy/tls/test-ca.key 4096
    openssl req -x509 -new -nodes -key deploy/tls/test-ca.key -sha256 -days 7 \
      -out deploy/tls/test-ca.crt -subj '/CN=Journey validation CA'
    openssl genrsa -out deploy/tls/server.key 2048
    openssl req -new -key deploy/tls/server.key -out deploy/tls/server.csr \
      -subj '/CN=validation.example.test'
    printf 'subjectAltName=DNS:validation.example.test\nextendedKeyUsage=serverAuth\n' \
      > deploy/tls/server.ext
    openssl x509 -req -in deploy/tls/server.csr -CA deploy/tls/test-ca.crt \
      -CAkey deploy/tls/test-ca.key -CAcreateserial -out deploy/tls/server.crt \
      -days 7 -sha256 -extfile deploy/tls/server.ext
    chmod 600 deploy/tls/test-ca.key deploy/tls/server.key

Import test-ca.crt into the browser or operating-system trust store before the
browser checks. The home process adds the mounted CA to its Rustls root store
while retaining normal chain and hostname verification.

## Fixtures and teardown

Use scripts/create-validation-fixtures.sh to create a valid JPEG and a
web-optimized MP4 of at least 50 MiB. Keep those files outside Git. After the
test, stop and remove both Compose projects, terminate the instance, release
the Elastic IP, remove DNS, revoke both credentials, delete deployment keys
and bundles, and remove the Docker context:

    docker context rm journey-aws
