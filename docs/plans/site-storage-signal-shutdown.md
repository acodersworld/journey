# Handle Ctrl-C in the site and storage apps

## Summary

Make `journey-site serve` and `journey-storage-service` exit cleanly on Ctrl-C.
Handle SIGTERM as well, so `docker compose stop` follows the same path.
Shutdown is prompt: active uploads and downloads are cancelled rather than
drained.

## Implementation

- Enable Tokio signal support in both apps and use one shutdown signal for
  SIGINT or SIGTERM. Treat a requested shutdown as a successful exit.
- In the site, stop the HTTP, storage WebSocket, and import-control listeners.
  Track and stop their spawned tasks, close the active storage session, and
  remove the control socket created by this process. Ensure Ctrl-C also works
  while the site waits for a direct h2c connection.
- In storage, stop the management UI and cancel whichever phase the outbound
  connection is in: connecting, serving requests, or waiting to reconnect.
  Abort active request tasks and drop the HTTP/2 session so its WebSocket
  closes. Existing unpublished PUT cleanup should remove partial files when
  their contexts are dropped.
- Report unexpected listener failures as errors during normal operation. Do
  not log task cancellation caused by the shutdown signal as a failure.

## Verification

- Send SIGINT and SIGTERM to each running binary while idle and during an
  active transfer. Verify prompt successful exit, closed listeners, and no
  lingering site control socket.
- Verify storage exits promptly while disconnected and retrying. Restart both
  apps after an interrupted upload and confirm the object store opens and the
  incomplete object is absent.
- Check that normal reconnect behavior and direct h2c mode still work without
  a shutdown signal.

## Assumptions

This covers the current site and storage deployment binaries. The older
gateway and home prototype apps are outside this change. "Stop immediately"
means cancelling active work rather than waiting for transfers to finish.
