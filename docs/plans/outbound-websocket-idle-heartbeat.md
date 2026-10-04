# Outbound WebSocket idle heartbeat

## Summary

An outbound WebSocket sends a WebSocket Ping control frame after 30 seconds
without HTTP/2 traffic in either direction. It ends the tunnel if the matching
WebSocket Pong control frame does not arrive within 10 seconds. Both durations
are configurable defaults.

## Implementation

- Keep all changes in `crates/journey-websocket`. Preserve the outbound
  WebSocket role so only outbound connections initiate heartbeats, regardless
  of their inner HTTP/2 role. Existing application call sites remain unchanged.
- Add positive `ping_interval` and `pong_timeout` values to the crate's
  `Config`, defaulting to 30 and 10 seconds.
- Reset the idle timer when a binary WebSocket message carrying HTTP/2 bytes
  is sent or received. On expiry, send a WebSocket Ping control frame with a
  unique payload through the bridge writer. Start the pong deadline after
  that frame is sent; bound a stalled send.
- Require a WebSocket Pong control frame with the matching payload to clear
  the deadline. Ordinary HTTP/2 traffic does not clear an outstanding ping.
  After the pong, restart the idle timer. On timeout or send failure, end the
  tunnel and let the existing application reconnect loop run.
- Continue replying to incoming WebSocket Ping control frames on both sides.
  Do not use HTTP/2 PING frames or application requests for the heartbeat.

## Verification

- Test default and custom timings, traffic postponing pings, matching and
  mismatched WebSocket Pongs, missing Pongs, and stalled sends.
- Confirm accepted WebSockets send no periodic Pings, HTTP/2 traffic remains
  unaffected, and application call sites compile unchanged.

## Assumptions

- WebSocket control frames do not count as HTTP/2 activity.
