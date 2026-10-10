# Basic operational logging for site and storage

## Summary

Add readable logging through `docker logs`, using standard Rust logging macros
and background output. Preserve queued records, with large bounded queues and
direct warnings when enqueueing is slow.

Batch storage range GETs into periodic summaries.

## Logging infrastructure

- Add a shared `journey-logging` workspace crate implementing a channel-backed
  `log::Log` backend. Both applications initialize it; libraries use standard
  `log` macros.
- Send owned records through a bounded channel to a dedicated output thread.
  That thread formats and writes stderr output, keeping blocking output off
  Tokio workers.
- Filter disabled levels before formatting and enqueueing. Capture timestamps
  when events occur.
- Use single-line text containing UTC timestamp, severity, event name, and
  escaped `key=value` fields. Disable ANSI colours.
- Add optional TOML settings to both applications:

  ```toml
  [logging]
  level = "info"
  queue_capacity = 65536
  slow_enqueue_warning_ms = 100
  ```

- Accept `off`, `error`, `warn`, `info`, `debug`, and `trace`. Require positive
  capacity and warning threshold. Existing configurations use these defaults;
  changes require restart, without rebuilding.
- Preserve records when the queue fills by waiting for capacity. Standard
  logging calls are synchronous, so this exceptional condition can block a
  calling Tokio worker.
- Measure enqueue duration. After an enqueue exceeds the configured threshold,
  write a warning directly to stderr, bypassing the queue. Include elapsed time
  and queue identity; rate-limit these warnings to once per queue every ten
  seconds.
- Replace operational print calls with logging macros. Preserve command output
  and startup errors before logger initialization.

## Application events and range aggregation

### Site

- Log request arrival and response readiness with local request ID, method, safe
  route template, status, and handler duration. Log streamed response failures
  separately.
- Keep successful static asset and media GET/HEAD requests at DEBUG.
- Log login success/rejection/throttling, logout, share-link
  creation/access/rejection/revocation, draft changes, publication, published
  edits, and media upload outcomes.
- Include relevant user, post, and block IDs. Login events may include attempted
  username and peer IP; do not blindly trust forwarded headers.
- Log startup, shutdown, storage connection changes, and reconnect failures.

### Storage

- Log starts and outcomes for full-object GET, HEAD, PUT, DELETE, LIST, and
  representation generation across HTTP/2 and management access. Include key,
  representation, status, duration, and local request ID where applicable.
- Generated-name PUT outcomes include the final key.
- Send range GET start/end events to a dedicated async aggregation task through
  a bounded channel, using the configured queue capacity. Use awaited sends
  when full; apply the same slow-enqueue warning policy.
- The aggregation task owns its map and timer. Share its sender across service
  clones and HTTP/2 connections.
- Group GETs carrying `Range` by object key and representation, regardless of
  content type. Different viewers and byte ranges belong to the same group.
- Add storage `[logging] range_get_summary_interval_seconds`, default **10**,
  requiring a positive value.
- Emit one INFO summary per group with activity: interval counts for started,
  completed, cancelled, failed, bytes handed to the response transport, and
  current active requests.
- Reset interval counters after emission and remove inactive groups once
  flushed. Keep individual range records at DEBUG; failures log immediately,
  and confirmed peer cancellations are counted in summaries.
- Record actual HTTP and streaming outcomes. Returning `Ok` from a handler must
  not classify an error response or truncated stream as successful.
- Do not send logging events per data chunk or HTTP/2 frame.

## Shutdown, verification, and compatibility

- Stop request producers, drain aggregation events, emit final summaries, then
  drain and join the output thread. Flush logging last during graceful shutdown.
- Test configuration defaults and validation, severity filtering, concurrent
  range grouping, interval boundaries, cancellation, reader failures, generated
  keys, and shutdown draining.
- With a deliberately small queue and slow test consumer, verify backpressure
  preserves records and produces a direct, rate-limited warning without
  recursive logging.
- Verify passwords, authorization headers, cookies, share secrets, raw
  URLs/query strings, and bodies never appear in logs.
- Document Docker follow/filter commands, configuration, summary meanings, and
  existing bounded retention.
- No schema or storage protocol changes. Preserve the filesystem abnormal-event
  journal and its stderr reporting.
- Queue draining protects graceful shutdown; abrupt termination can still lose
  records held in memory.
