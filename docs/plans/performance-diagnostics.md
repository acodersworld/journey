# Performance diagnostics for site and storage

## Summary

Add three repeatable download tests:

```text
Site CLI:          storage → existing connection → site → discard
curl to site:      site sample file → HTTP response → curl
curl through site: storage sample file → existing connection → site → curl
```

Use a temporary **10 MiB random sample file**, created on demand, with no large
application buffer retained between tests.

## Source organization

- Keep the bulk of the diagnostics implementation in dedicated source files.
  Do not add large benchmark, sample-generation, or diagnostic-handler
  implementations to the existing application entrypoints, web module,
  storage client module, control module, or HTTP/2 service module.
- Use dedicated site performance modules for CLI arguments and execution,
  benchmark coordination, timing, result types, and output formatting. Put
  website diagnostic handlers in a separate web submodule.
- Put storage diagnostic route handling, sample-file lifecycle, and repeated
  sample streaming in dedicated storage modules. Separate substantial shared
  sample-file helpers from benchmark orchestration and HTTP handlers.
- Modify existing files for integration: module declarations, configuration,
  CLI and control-command dispatch, route registration, state/lifecycle wiring,
  and small transport adapters. Keep these edits targeted and delegate the
  diagnostics work to the new modules.
- Keep feature-specific tests with the dedicated modules or in separate test
  files. Existing test modules should gain only the integration and regression
  checks needed to verify the hooks.
- Review the final diff for this separation as an implementation acceptance
  requirement. Preserve existing formatting and avoid unrelated refactoring.

## Sample files and streaming

- Site and storage each lazily create one private temporary sample file outside
  the object namespace. Generate it once per process, coordinating concurrent
  preparation requests.
- Complete preparation before timed tests. Retain the file for subsequent tests
  and remove it on graceful shutdown.
- Read through small, per-transfer buffers. Repeat the sample to produce the
  requested response length, defaulting to **256 MiB**. Never generate an
  endless response or allocate the entire transfer in memory.
- Use independent reader offsets for concurrent transfers. Respect transport
  backpressure and stop reading when the client disconnects.
- Send `application/octet-stream`, an accurate Content-Length, and cache-control
  directives preventing caching and transformation. Benchmark clients request
  identity encoding.
- Label results as sample-file tests: repeated reads can use the OS page cache,
  so these do not measure uncached physical disk throughput.

## Interfaces and access

- Add optional `[diagnostics] enabled = false` to both applications. Existing
  deployments remain unchanged until diagnostics are explicitly enabled.
- Add the storage diagnostic route `GET /perftest/?bytes=<COUNT>` and
  `HEAD /perftest/?bytes=<COUNT>`, served through the existing HTTP/2 connection.
  The path is `/perftest/`, including the trailing slash, outside `/objects/`.
  `bytes` is optional, defaults to 268435456 (256 MiB), and must be positive
  when supplied. HEAD prepares the sample without sending its payload; GET
  streams the requested number of bytes from the repeated sample.
- Add site admin-only GET/HEAD endpoints for downloading its own sample and
  proxying the storage sample. Both accept a positive `bytes` query parameter;
  successful HEAD prepares the relevant source.
- Extend the site storage client to request the diagnostic sample over its
  currently configured transport using `/perftest/`. Preserve the normal
  object interfaces and storage isolation; add no network listener.
- Add `journey-site performance storage --bytes <SIZE> --concurrency <N>`,
  defaulting to 256 MiB and one transfer.
- Accept positive integer sizes with case-insensitive suffixes, following the
  existing CLI window-size conventions: no suffix or `B` means bytes;
  `K`/`KiB`, `M`/`MiB`, and `G`/`GiB` use powers of 1024; `KB`, `MB`, and `GB`
  use powers of 1000. Examples: `--bytes 512K`, `--bytes 10MB`, and
  `--bytes 256MiB`. Parse into a byte count with checked `u64` arithmetic,
  rejecting zero, fractional sizes, unsupported suffixes, and overflow. Do not
  apply the HTTP/2 window-size limit to transfer lengths. Send the resulting
  integer byte count over the control socket and in HTTP query parameters.
- The CLI sends a command through the existing private control socket. The
  running site prepares the storage sample, then performs and drains the
  downloads through its actual shared storage connection.
- Also support `journey-site performance storage --key <KEY> --concurrency <N>`
  to download an existing object completely. This mode uses normal object GETs
  and does not repeat or modify the object.
- Preserve compatibility with the existing control-socket import request
  format.
- Use existing admin login/session authentication for curl endpoints. Disabled
  diagnostics and non-admin requests must not initiate preparation or storage
  traffic.

## Measurements and operation

- CLI results report source, transport, requested concurrency, completed
  transfers, bytes actually received, time to first body bytes, total duration,
  aggregate MiB/s, and failures. Offer readable output and `--json`.
- Report throughput from bytes consumed at the receiving site, rather than
  bytes merely handed to storage's transport. Report effective site HTTP/2
  receive-window settings alongside results.
- Document preparation with HEAD followed by timed GETs using curl's
  [write-out metrics](https://curl.se/docs/manpage.html), discarding bodies to
  `/dev/null`.
- Run comparisons with one transfer and four concurrent transfers, through the
  public site URL and directly against the site listener where available.
- Include a comparison using a real image's normal authenticated media URL.
  This exercises normal authorization and image-serving paths.
- Log preparation, test start, and final outcome without per-chunk messages.
  Support cancellation and existing graceful shutdown; do not tune window
  settings automatically.

### Curl test script

- Provide a curl-based test script accepting a website test endpoint URL, an
  existing admin cookie-jar file, transfer size, and concurrency. Use the same
  size suffix conventions as the CLI. Default to 256 MiB and one transfer;
  document running each website test endpoint with concurrency 1 and 4.
- Document obtaining the cookie jar through `POST /api/auth/login`, sending
  JSON credentials and the site's allowed `Origin` header, and saving cookies
  with curl's `--cookie-jar`. Keep login outside benchmark timing. The runner
  consumes an existing cookie jar and does not print credentials or cookies.
- Convert the requested size to an integer byte count. Issue one authenticated
  HEAD request to the chosen endpoint with that `bytes` count, and require a
  successful response before starting the timed GETs. Preparation is excluded
  from benchmark results.
- For every GET, use `--fail --silent --show-error`, the cookie jar,
  `Accept-Encoding: identity`, and `--output /dev/null`. Do not follow login
  redirects. Require HTTP 200 and the expected downloaded byte count; a curl
  error, error response, or truncated payload fails the test.
- Collect curl's write-out fields `http_code`, `size_download`, `time_total`,
  `time_starttransfer`, and `speed_download` in machine-readable records.
  `size_download` counts downloaded body bytes, excluding headers;
  `speed_download` is curl's reported average download speed in bytes/sec.
- Calculate a separately named end-to-end rate as shown below. It includes
  connection setup and response waiting. Keep curl's reported speed as a
  separate metric rather than assuming the two calculations are identical.

  ```text
  end_to_end_bytes_per_second = size_download / time_total
  end_to_end_mib_per_second   = end_to_end_bytes_per_second / 1048576

  Example: 268435456 bytes / 4 seconds = 67108864 bytes/sec = 64 MiB/sec
  ```

- Report `time_starttransfer` as response time to first byte, not time to first
  body chunk. It includes connection preparation and waiting for the server's
  response. This distinguishes slow response starts from slow downloads.
- For concurrent tests, start the requested number of curl transfers together
  and measure batch duration using a monotonic clock from immediately before
  launching them until all have finished. Calculate aggregate throughput as
  total downloaded bytes divided by batch duration, then divide by 1048576 for
  MiB/sec. Do not sum individual rates or durations. Any failed transfer marks
  the batch as failed; retain its measurements for diagnosis.
- Print a readable result summary and support JSON output containing the
  per-transfer fields, curl exit statuses, batch elapsed seconds, total bytes,
  aggregate bytes/sec and MiB/sec, and overall success/failure. Exit nonzero
  when preparation or any transfer fails.
- Include the following single-transfer command in the usage documentation.
  `TEST_URL` is the selected website endpoint, without an existing query string;
  run the preparation HEAD request first.

  ```bash
  curl --fail --silent --show-error \
    --cookie cookies.txt \
    --header 'Accept-Encoding: identity' \
    --output /dev/null \
    --write-out 'status=%{http_code}\nbytes=%{size_download}\nseconds=%{time_total}\nbytes_per_sec=%{speed_download}\nttfb_seconds=%{time_starttransfer}\n' \
    "$TEST_URL?bytes=268435456"
  ```

## Verification and compatibility

- Verify preparation happens once, temporary files remain outside stored
  objects, and transfer lengths are exact below, at, and above the sample size.
- Test concurrent readers, cancellation, unavailable storage, disabled
  diagnostics, and authorization failures.
- Test CLI size suffixes, case-insensitive parsing, decimal versus binary
  units, bare byte counts, and rejection of invalid or overflowing sizes.
- Verify CLI tests use the running site's connection and both configured
  storage transports work.
- Verify existing import commands, object operations, and shutdown remain
  functional.
- Verify the curl runner excludes preparation, validates status and byte
  counts, reports failed or truncated transfers, and calculates concurrent
  throughput from batch wall-clock duration rather than summed rates.
- Perform a release-build smoke test of all three paths. Interpret comparisons
  as diagnostic evidence, without imposing a hardware-specific throughput
  threshold.
- No database migration, browser diagnostics page, or normal image-delivery
  changes are included.
