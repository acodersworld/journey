# AWS/home real-world validation result

This is a template. Copy it to a dated, private result record after the
complete disposable deployment. Do not commit the completed record when it
contains deployment details, logs, or generated measurements.

## Identity

- Date/time and timezone:
- Git commit:
- Image tag:
- journey-websocket dependency versions:
- Test domain (no credentials):
- Final decision: PASS / FAIL

## Infrastructure

- AWS instance type:
- AMI:
- Root volume:
- Docker and Compose versions:
- Home host architecture:
- Home Docker version:
- Security-group and DNS teardown completed:

## Fixtures and upload

| Object | Path/name | Size | SHA-256 |
|---|---|---:|---|
| JPEG |  |  |  |
| MP4 |  |  |  |
| 256 MiB upload | digest-named file | 268435456 |  |

## Automated checks

- Missing and invalid site credentials:
- Home credential rejected by site routes:
- Site credential rejected by WebSocket route:
- Health:
- Complete JPEG and MP4:
- HEAD metadata:
- Beginning/middle/end/open-ended/suffix ranges:
- Malformed/multiple/unsatisfiable ranges:
- Upload response and stored digest:
- Concurrent throttled video and image:
- Cancellation followed by health/media:
- Repeated large transfers and temporary-file cleanup:

## Manual checks

- Browser trusted the private CA without a warning:
- Page loaded after site challenge:
- JPEG rendered:
- MP4 played:
- Seek beginning/middle/end:
- Upload displayed size and digest:
- Cancelled upload/navigation recovered:

## Lifecycle checks

- Home outage failure time:
- Home recovery time:
- Gateway restart reconnect time:
- Nginx restart reconnect time:
- Any manual intervention required:

## Resource measurements

Record docker stats --no-stream, restart counts, OOM state, and relevant host
memory at each point. Include the raw sanitized output as an attachment outside
Git.

| Point | Gateway RSS | Nginx RSS | Home RSS | Restarts/OOM | Notes |
|---|---:|---:|---:|---|---|
| Idle connected |  |  |  |  |  |
| Complete video |  |  |  |  |  |
| Range |  |  |  |  |  |
| 256 MiB upload |  |  |  |  |  |
| Throttled video + image |  |  |  |  |  |
| Cancelled transfer |  |  |  |  |  |
| Outage/reconnect |  |  |  |  |  |
| Restarts |  |  |  |  |  |
| Repeated transfers |  |  |  |  |  |

Acceptance summary:

- Gateway peak RSS below 160 MiB:
- Nginx peak RSS below 48 MiB:
- Home stayed within 256 MiB:
- No unexpected restart or OOM:
- Gateway returned within 20 MiB of idle:
- AWS disk did not grow with proxied objects:
- Slow client showed backpressure:

## Deviations and retests

| Check | Failure/deviation | Fix | Retest | Result |
|---|---|---|---|---|
|  |  |  |  |  |

## Sanitized logs and sign-off

- Relevant log excerpts:
- Unresolved transport defects:
- AWS resources destroyed:
- Credentials revoked:
- CA private key and deployment bundle deleted:
- Reviewer:
