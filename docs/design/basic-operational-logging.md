# Basic operational logging

The site and storage processes install one shared `journey-logging` backend.
Libraries emit through the `log` facade. The backend filters levels before
formatting and writes each record directly to stderr while holding the stderr
lock. Output is UTC, single-line text with a service identity, severity, event
name, and escaped fields. It uses no ANSI colors, log channel, or writer
thread. A slow stderr write blocks the calling thread and can block a Tokio
worker.

The optional `[logging]` table defaults to `level = "info"`. Supported levels
are `off`, `error`, `warn`, `info`, `debug`, and `trace`. The former
`queue_capacity` and `slow_enqueue_warning_ms` keys remain accepted but are
ignored so existing TOML files continue to load.

Site request logs use process-local request IDs and safe route templates.
When the route is `unmatched`, a quoted `path` field shows the requested path
without query strings. Unmatched paths in the `/share` namespace are logged as
`/share/[redacted]`, including malformed paths with repeated slashes or encoded
namespace characters and separators. This protects share-link secrets and
WhatsApp preview image tokens. Other unmatched paths are limited to 2,048
characters with a `[truncated]` marker when longer. Paths are escaped for
single-line output.
They record arrival and response readiness, including method, status, and
handler duration. Successful static assets and media GET/HEAD requests use
DEBUG. Response body errors are separate events. Business events record
account, post, block, and share-link identifiers where relevant. Login events
may record the attempted username and the peer address obtained from the
socket; forwarded headers are not used. Full request URLs, query strings,
request bodies, passwords, cookies, authorization headers, and share secrets
are not logged. The Nginx access format records time, method, status, and request time
without a URI. Nginx error logging is disabled because some error messages
include the original request line.

Storage HTTP/2 and management operations record local request IDs, method,
safe key, representation, response status, and duration. Generated PUT logs
include the final generated key. Management streaming failures are logged as
they occur. A response is not classified as successful merely because its
handler returned; HTTP status and stream failures are tracked separately.

Ranged HTTP/2 GETs update a shared in-process map under a short mutex. The
summary task snapshots that map at each interval, releases the lock, and then
writes summaries directly. It groups by object key and representation,
combining viewers and byte ranges, then reports interval counts for started,
completed, cancelled, failed, bytes handed to the response transport, and
`active_requests`. `requests_total` accumulates starts for that key and
representation during the current activity burst, so it remains visible on
each interval for a long-running stream. The group is removed after a summary
finds no active requests, which bounds memory and resets the total for a later
burst.
Individual range events are DEBUG; interval summaries are INFO. The storage
interval defaults to ten seconds and must be positive. Inactive groups are
removed after a summary.

On graceful storage shutdown, accepted request handlers drain before the
summary task emits its final summaries. Log records have already been written
to stderr synchronously. Abrupt termination can omit range counts that have
not yet reached a summary interval. No database schema or storage protocol
changes are part of this design. The filesystem abnormal-event journal keeps
its existing stderr reporting. Compose retains two 2 MiB JSON log files per
container.
