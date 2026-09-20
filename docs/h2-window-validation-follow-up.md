# HTTP/2 Window Validation Follow-up

**Status:** Complete
**Created:** 20 September 2026
**Component:** `crates/journey-websocket`
**Related commit:** `7a3fb73` (`Apply back pressure from server on queue full`)

## 1. Purpose

The request-admission and client-readiness changes in commit `7a3fb73` work as
intended. Review found one incomplete validation rule and one unnecessarily
restrictive rule in the new HTTP/2 window configuration.

This document records those findings and the changes needed to resolve them.

## 2. Finding: oversized windows can panic

### Current behavior

`Config::validate()` verifies that `h2_initial_stream_window_size` is nonzero,
but it does not impose the HTTP/2 maximum window size. It also does not impose
that maximum on `h2_initial_connection_window_size`.

Both fields are `u32`, so callers can supply values greater than the HTTP/2
maximum of `2^31 - 1`, or `0x7fff_ffff`.

The values later reach the `h2` builders through:

- `client::Builder::initial_window_size`
- `client::Builder::initial_connection_window_size`
- `server::Builder::initial_window_size`
- `server::Builder::initial_connection_window_size`

The `h2` crate asserts the maximum connection-window size when establishing or
configuring the connection. An invalid public `Config` can therefore panic the
process instead of returning `Error::Configuration`. An oversized stream
window can also produce an invalid HTTP/2 setting and fail the connection at
the protocol boundary.

### Required behavior

Reject both window values during `Config::validate()` unless each is within
the crate's supported nonzero range:

```text
1..=2_147_483_647
```

HTTP/2 permits a zero initial stream window, but this crate rejects zero so a
new stream can transfer DATA without relying on a later window update. Every
unsupported value must produce `Error::Configuration` before any WebSocket or
HTTP/2 handshake work begins.

### Recommended implementation

Define a local constant near the other configuration defaults:

```rust
const MAX_H2_WINDOW_SIZE: u32 = (1 << 31) - 1;
```

Then validate the fields independently:

```rust
if !(1..=MAX_H2_WINDOW_SIZE).contains(&self.h2_initial_stream_window_size) {
    return Err(Error::Configuration(
        "HTTP/2 stream window size must be between 1 and 2^31 - 1",
    ));
}

if !(1..=MAX_H2_WINDOW_SIZE).contains(&self.h2_initial_connection_window_size) {
    return Err(Error::Configuration(
        "HTTP/2 connection window size must be between 1 and 2^31 - 1",
    ));
}
```

Using an internal constant avoids depending on a private constant from `h2`
and documents the upper protocol constraint at the public configuration
boundary. The lower bound is an intentional crate policy.

## 3. Finding: the two windows need not be ordered

### Current behavior

`Config::validate()` rejects this relationship:

```text
h2_initial_connection_window_size < h2_initial_stream_window_size
```

The field documentation likewise states that the connection window must be at
least as large as the stream window.

### Why this is unnecessarily restrictive

HTTP/2 stream and connection flow-control windows are independent limits.
Sending DATA requires capacity in both windows, but the protocol does not
require the connection window to be greater than or equal to every stream
window.

A smaller connection window is useful when the application wants to permit a
larger burst on any one stream while maintaining a tighter aggregate in-flight
DATA limit across the connection. The connection window simply becomes the
effective constraint until capacity is released.

### Required behavior

- Remove the comparison between the stream and connection windows.
- Validate each field independently against the crate's supported nonzero
  range `1..=2^31 - 1`.
- Remove the statements from the field documentation that one value must be
  greater than or equal to the other.

For example, this must be accepted:

```rust
Config {
    h2_initial_stream_window_size: 1024 * 1024,
    h2_initial_connection_window_size: 512 * 1024,
    ..Config::default()
}
```

## 4. Tests to add or update

Extend `invalid_http2_and_request_queue_limits_are_rejected` or split the
window cases into a focused test.

Verify that validation rejects:

- `h2_initial_stream_window_size == 0`
- `h2_initial_connection_window_size == 0`
- `h2_initial_stream_window_size == 0x8000_0000`
- `h2_initial_connection_window_size == 0x8000_0000`
- `u32::MAX` for either window

Verify that validation accepts:

- Both windows equal to `1`
- Both windows equal to `0x7fff_ffff`
- A connection window larger than the stream window
- A connection window smaller than the stream window

At least one public session entry-point test should also confirm that an
oversized window returns `Error::Configuration` before beginning handshake
work. This protects the important behavior—not merely the private validation
implementation—from regression.

## 5. Documentation updates

After implementing the change:

- Update the `Config` field documentation in `journey-websocket/src/lib.rs`.
- Record the completed correction in
  `docs/h2-over-websocket-hardening-checklist.md`.
- Keep the extraction review's statement that HTTP/2 limits are explicit, but
  describe `1..=2^31 - 1` as the crate's supported nonzero range.
- Document that the connection setting is a target: HTTP/2 connection credit
  starts at 65,535 bytes, and a lower target takes effect only as received
  capacity is released.
- Mark this document's status as `Complete` and add the verification results.

## 6. Verification

Run the smallest relevant test first, followed by:

```text
cargo test --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
git diff --check
```

Do not run `cargo fmt`, `cargo fmt --check`, `rustfmt`, or another automated
source formatter in this repository.

Docker verification is not required for this narrowly scoped validation fix.

## 7. Acceptance criteria

This follow-up is complete when:

- No public window setting can make `h2` panic because it exceeds the protocol
  maximum.
- Invalid window values return `Error::Configuration` before handshake work.
- Stream and connection windows may be configured independently.
- Boundary and relationship tests pass.
- The complete workspace test suite and strict Clippy pass.

## 8. Verification

Complete. `Config::validate()` now independently enforces the crate's supported
nonzero HTTP/2 window range `1..=2^31 - 1`, and the stream/connection ordering
requirement has been removed. The documentation distinguishes that policy from
the protocol's allowance for a zero stream window and describes the connection
setting as a target whose credit initially starts at 65,535 bytes. Focused
window and public-entry-point tests, the complete workspace test suite, strict
Clippy, and `git diff --check` pass. No automated formatter was run.
