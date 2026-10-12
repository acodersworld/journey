# Performance diagnostics

Both applications accept an optional `[diagnostics]` table with `enabled = true`.
The default is `false`. Enable it in both configurations and restart both
processes to compare all three paths. There is no database change or extra
listener. Storage serves `GET` and `HEAD /perftest/?bytes=N` through its existing
HTTP/2 connection; the site exposes authenticated admin endpoints
`/api/perftest/site` and `/api/perftest/storage`.

The storage crate's `h2c_get_server` example enables `/perftest/` by default
for both in-memory and filesystem stores. No diagnostics flag or configuration
is needed for that example; its startup output includes curl test commands.

Each process lazily creates one private, random 10 MiB file in its temporary
directory. It is deleted when the process releases it. GET repeats that file
for exactly `N` bytes (256 MiB by default); HEAD prepares it without a body.
Transfers use per-request readers and small buffers. The sample can be served
from the OS page cache, so these results do not represent uncached disk speed.

## Site CLI

Run through the private control socket of the serving site:

```sh
journey-site --config site.toml performance storage --bytes 256MiB --concurrency 1
journey-site --config site.toml performance storage --bytes 256MiB --concurrency 4 --json
journey-site --config site.toml performance storage --key media/EXISTING_KEY --concurrency 1
```

The site first prepares the storage sample with HEAD, then starts the requested
GETs together over its shared storage connection and drains each body. Object
mode uses normal object GETs without changing the object. CLI results count
bytes received at the site and report first body bytes, total elapsed time,
aggregate MiB/s, failures, transport, and effective receive windows. Results
with failures exit nonzero. Sizes accept a bare byte count, `B`, binary
`K`/`KiB`, `M`/`MiB`, `G`/`GiB`, and decimal `KB`, `MB`, `GB`, ignoring case.

## Curl tests

Create an admin cookie jar before timing. Use the site's configured allowed
Origin and an admin account; enter credentials locally, without placing them
in a committed script or benchmark output:

```sh
curl --fail --silent --show-error \
  --header 'Origin: https://YOUR_SITE_ORIGIN' \
  --header 'Content-Type: application/json' \
  --data '{"username":"ADMIN","password":"PASSWORD"}' \
  --cookie-jar cookies.txt \
  https://YOUR_SITE_ORIGIN/api/auth/login
```

Run the script against each endpoint, with one and four transfers. Use the
public URL, then the site listener directly where reachable. Keep the same
size and transport settings for each comparison:

```sh
python3 scripts/performance-curl.py https://YOUR_SITE_ORIGIN/api/perftest/site cookies.txt --bytes 256MiB --concurrency 1
python3 scripts/performance-curl.py https://YOUR_SITE_ORIGIN/api/perftest/site cookies.txt --bytes 256MiB --concurrency 4 --json
python3 scripts/performance-curl.py https://YOUR_SITE_ORIGIN/api/perftest/storage cookies.txt --bytes 256MiB --concurrency 1
python3 scripts/performance-curl.py https://YOUR_SITE_ORIGIN/api/perftest/storage cookies.txt --bytes 256MiB --concurrency 4
```

The runner sends one authenticated HEAD request before its timed GETs and
requires HTTP 200. Every GET uses `--fail --silent --show-error`, the cookie
jar, identity encoding, and `/dev/null` output. It never follows login
redirects. It requires HTTP 200 and the requested body byte count, retaining
failed transfer metrics and exiting nonzero. `time_starttransfer` is curl's
response start time; it includes connection setup and server waiting, so it is
different from the CLI's first body byte time. `speed_download` is curl's
reported average bytes/s. The separately named end-to-end rate is
`size_download / time_total`; aggregate batch MiB/s is total downloaded bytes
divided by monotonic batch elapsed seconds and 1,048,576.

For a single manual transfer, send a HEAD request first, then run:

```bash
curl --fail --silent --show-error \
  --cookie cookies.txt \
  --header 'Accept-Encoding: identity' \
  --output /dev/null \
  --write-out 'status=%{http_code}\nbytes=%{size_download}\nseconds=%{time_total}\nbytes_per_sec=%{speed_download}\nttfb_seconds=%{time_starttransfer}\n' \
  "$TEST_URL?bytes=268435456"
```

As a separate comparison, use a real image's normal authenticated media URL
with curl and the same cookie jar, writing its body to `/dev/null`. This path
includes normal authorization and image serving. Use curl's downloaded byte
count and total time for the rate; the image size need not match the sample
length. These comparisons are diagnostic evidence, not pass/fail thresholds.
