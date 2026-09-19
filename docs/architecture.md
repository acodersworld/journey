# Journey: Architecture

**Status:** Exploratory draft  
**Updated:** 19 September 2026  
**Purpose:** Record the intended production direction and unresolved questions. This remains an exploratory architecture, not a commitment to implement every component immediately.  
**First implementation:** [Vertical Slice 01: HTTP/2 over WebSocket Object Download](vertical-slice-01-http2-over-websocket.md)  
**Implementation plan:** [Checkpoint 1: Transferable HTTP/2 Transport Boundary](vertical-slice-01-implementation-plan.md)

## 1. Project idea

Journey is an online publishing system conceptually similar to WordPress. It will use a small public-facing AWS server for the website and a more capable home server for durable picture and video storage.

The AWS instance is deliberately small, initially assumed to have about 1 GiB of RAM. The home server is behind a residential router and must not require a publicly exposed inbound port.

## 2. Current goals

- Run the public website, application logic, and content database on AWS.
- Serve the public website over HTTPS with HTTP/2, while retaining HTTP/1.1 fallback where needed.
- Use React for browser-side interactivity without running Node.js in production.
- Keep original pictures and videos on the home server.
- Let AWS store and retrieve media through a narrow, purpose-built interface.
- Support progressive video playback and seeking through standard public HTTP range requests.
- Require the home server to initiate the connection across the home network boundary.
- Use one persistent home connection with a small bounded number of concurrent object streams.
- Keep the AWS memory footprint predictable.
- Continue serving already-cached public media when home is unavailable.
- Make the first prototype simple enough to understand, test, and replace.

## 3. Non-goals for the first prototype

- WordPress plugin compatibility.
- A VPN or general-purpose network tunnel.
- Shell access to the home server from AWS.
- Arbitrary remote file paths or filesystem operations.
- Unbounded concurrent streams or transfer queues.
- Resumable or application-level chunked transfers.
- Multi-region operation or automatic AWS failover.
- High availability for the home server.
- Final decisions about the editor experience, themes, accounts, or publishing workflow.

## 4. Proposed high-level architecture

```text
Browser
      |
      | HTTPS: HTTP/2 preferred
      | HTML, API, pictures, video ranges/segments
      v
+--------------------------------+
| AWS micro instance             |
|                                |
|  Nginx + compiled React assets |
|    |                           |
|  Rust API + SQLite             |
|    |                           |
|  Bounded public-media cache    |
|    |                           |
|  HTTP/2-over-WS gateway        |
+---------------^----------------+
                |
                | one persistent, authenticated WSS connection
                | carrying a multiplexed HTTP/2 session
                |
+---------------|----------------+
| Home server                    |
|                                |
|  Journey home agent            |
|  Inner HTTP/2 object server    |
|    |                           |
|  Temporary writes + validation |
|    |                           |
|  Original-media storage        |
|  Media processing              |
|  Media and DB backups          |
+--------------------------------+
```

The current leading design carries a restricted HTTP/2 object API inside one WebSocket Secure (WSS) connection. The home process connects outward to AWS and remains connected. A byte-stream adapter hides WebSocket message boundaries from HTTP/2. AWS becomes the inner HTTP/2 client and home becomes the inner HTTP/2 server, regardless of which side initiated the outer connection.

Public browser HTTP/2 and private storage HTTP/2 are independent connections. Nginx negotiates HTTP/2 with browsers using TLS ALPN and upgrades the home connection to WebSocket. The inner HTTP/2 session starts directly over the authenticated WebSocket byte stream without another TLS or ALPN exchange.

The home HTTP/2 server is not a general proxy. It exposes only a strict object API using immutable identifiers. This is intentionally narrower than WireGuard, Tailscale, Chisel, or a reverse SSH tunnel. Yamux plus a custom object protocol remains an alternative if HTTP/2 proves disproportionate in the first vertical slice.

### Frontend build and runtime

React runs in each visitor's browser. The React source, TypeScript, CSS, and dependencies are compiled into versioned static HTML, JavaScript, CSS, and asset files before deployment.

The production container build uses multiple stages:

1. A pinned Node.js build stage installs the exact dependency versions from the committed lockfile using `npm ci`.
2. The Node stage runs the frontend production build and produces a `dist` directory.
3. Only the compiled `dist` files are copied into the final Nginx image or other static-asset image.
4. The Node build stage and `node_modules` are absent from the production runtime image.

Frontend images should normally be built on a development machine or in CI and then deployed to AWS. Running the build on the 1 GiB production instance would create avoidable memory and CPU pressure.

Nginx serves fingerprinted frontend assets with long-lived immutable cache headers and routes dynamic API requests to the Rust application. React requests data on demand; for example, infinite scrolling calls a cursor-paginated endpoint such as `GET /api/posts?limit=20&after=<cursor>` and appends the returned records in the browser.

Node.js is therefore a build-time dependency only. It is not a production service and consumes no runtime memory on AWS. The initial rendering and search-indexing strategy for public post pages remains an open product decision; Rust-rendered public HTML and a React administration/editor application remain compatible with this build model.

## 5. Connection model

### Establishment

1. The home client opens `wss://www.example.com/internal/files` through Nginx.
2. It validates the normal public TLS certificate for the AWS hostname.
3. The WebSocket HTTP upgrade includes dedicated HTTP Basic credentials in the `Authorization` header, never in the URL.
4. Nginx or the Journey application validates Basic authentication before accepting the WebSocket upgrade.
5. After the upgrade, AWS issues a one-time SSH-style public/private-key challenge.
6. The home client proves possession of a dedicated Ed25519 private key by signing the challenge transcript.
7. AWS verifies the signature against the pinned home-client public key.
8. Both sides switch the authenticated WebSocket to binary byte-stream mode.
9. AWS performs the inner HTTP/2 client handshake and home performs the server handshake.
10. AWS marks the storage connection ready and accepts only one ready home-storage session for the account.
11. HTTP/2/WebSocket liveness checks detect a dead connection; home reconnects with bounded exponential backoff and jitter.

The long-lived connection is full duplex. AWS opens bounded concurrent HTTP/2 streams for authorized object operations. No HTTP/2 preface or object operation is accepted until both authentication stages have succeeded.

### SSH-style key challenge

The second stage uses the public/private-key idea familiar from SSH: home proves that it holds a private key without ever sending that key to AWS. It is implemented as an application-level Ed25519 challenge-response, not as an SSH connection or a partial implementation of the SSH protocol:

1. AWS generates a fresh 32-byte random nonce and a unique connection ID.
2. AWS sends a bounded `challenge` message containing the nonce, connection ID, protocol version, and a short expiry.
3. Home signs an unambiguous binary encoding of the context string `journey-ws-auth-v1`, nonce, connection ID, and negotiated protocol version.
4. Home returns its key ID and signature in a bounded `proof` message.
5. AWS looks up the pinned public key, verifies the expiry and signature, consumes the challenge, and returns `ready`.

The nonce is single-use and exists only for that WebSocket connection. Including the connection ID, version, and domain-separation string prevents a captured proof from being replayed on another connection or interpreted as a signature for another purpose. Authentication failures close the connection with a generic error and are rate-limited.

The home client should have a new Ed25519 key created only for Journey. The private key remains on the home server; AWS stores only the corresponding public key. Journey must not reuse the private key that grants interactive SSH access. An OpenSSH key file format may be used for storage if the selected cryptographic library reads it, but SSH itself is not run over the WebSocket.

### Public exposure

Only ports 80 and 443 need to be public on AWS. Nginx handles public HTTPS and upgrades the internal endpoint to WebSocket. The endpoint may use a separate hostname, but it does not require one.

A long, unguessable URL can reduce random scanning noise, but it is not authentication. The credential and server-side authorization remain mandatory.

The home server exposes no inbound internet port. Its file service does not listen for requests from the LAN or internet unless a separate local administration interface is deliberately added.

## 6. Private object protocol

### Transport layering

```text
Restricted object API
        ↓
HTTP/2 streams and DATA frames
        ↓
WebSocket byte-stream adapter
        ↓
Authenticated WSS
        ↓
TCP
```

The adapter converts bounded binary WebSocket messages into the continuous ordered byte stream expected by HTTP/2. WebSocket message boundaries carry no application meaning. Ping, pong, close, invalid text messages, buffering, backpressure, flush, and shutdown behavior are handled explicitly by the adapter.

HTTP/2 supplies multiplexing, per-stream cancellation, response status, headers, streaming bodies, and flow control. The gateway must still bridge backpressure correctly and impose strict limits on concurrent streams, header sizes, ranges, file handles, tasks, and buffers.

### Object identity

Objects are immutable and addressed by an identifier derived from their content:

```text
sha256:<64 lowercase hexadecimal characters>
```

The API never accepts an absolute path, relative path, filename, arbitrary URL, or proxy destination. The home service maps a validated object ID to its own internal storage layout.

### Initial API

The first safe read surface is:

```http
GET  /objects/sha256/<digest>
HEAD /objects/sha256/<digest>
```

A `GET` may include one valid `Range: bytes=...` header. Home returns normal bounded HTTP statuses and headers, including `200`, `206`, `400`, `404`, `405`, and `416` as appropriate.

`PUT` is added only after reads, streaming, cancellation, backpressure, and reconnect behavior have been validated. `DELETE`, `LIST`, arbitrary paths, arbitrary authorities, and arbitrary query strings are omitted from the initial protocol.

## 7. Picture upload flow

1. An authenticated author uploads a picture to the Journey application on AWS.
2. AWS enforces an upload-size limit and stages the data in a temporary file rather than application memory.
3. AWS calculates the SHA-256 digest and derives the immutable object ID.
4. When the authenticated home connection is ready, AWS opens an inner HTTP/2 stream and sends `PUT /objects/sha256/<digest>` with a streaming request body.
5. Home streams HTTP/2 DATA into a temporary file inside the object-storage volume while enforcing length and quota limits.
6. Home checks the declared size and SHA-256 digest.
7. Home atomically renames the valid temporary file into its final internal location.
8. Home returns a successful HTTP status only after durable storage; AWS then records the object ID in its database.

If the object already exists with the correct size and digest, `put` succeeds without creating a duplicate. This makes a whole-operation retry idempotent.

The lifetime and maximum size of the AWS staging file need explicit limits. AWS must not discard its only copy until home has confirmed durable storage. If home is authoritative, home storage also needs a separate backup.

## 8. Picture request flow

1. A browser requests a public picture from AWS.
2. Journey validates the public request and looks for the required immutable variant in its local cache.
3. On a cache hit, AWS serves the file immediately.
4. On a miss, AWS opens a bounded inner HTTP/2 `GET` stream to home.
5. Home returns bounded metadata and streams the object through HTTP/2 DATA frames.
6. AWS streams it into a temporary cache file while enforcing the expected size and object identity.
7. AWS atomically publishes the valid cache entry and serves it to the browser.

HTTP/2 permits a small configured number of concurrent object streams so a large transfer does not wait for or completely block a small picture. The AWS queue remains bounded, duplicate requests for the same object should be coalesced, and public requests have explicit timeouts.

## 9. Video streaming

### Public delivery

Journey should serve the whole public website over HTTPS with HTTP/2 preferred. Browsers may fall back to HTTP/1.1 without changing application behaviour. HTTP/2 allows HTML, scripts, pictures, API responses, and video data to share a multiplexed connection efficiently.

HTTP/2 does not itself define video seeking. Public video endpoints must implement normal HTTP byte-range behaviour:

- Advertise `Accept-Ranges: bytes`.
- Parse and validate a single bounded `Range` request initially.
- Return `206 Partial Content` with a correct `Content-Range` and `Content-Length`.
- Return `416 Range Not Satisfiable` for invalid or unsatisfiable ranges.
- Stream from disk through bounded buffers rather than loading a video into RAM.
- Support request cancellation when a viewer pauses, leaves, or seeks elsewhere.

For ordinary MP4 delivery, generated files should be web-optimized so their metadata is available near the beginning of the file. This permits playback to start without downloading the entire object.

### Retrieving video from home

AWS maps a validated public browser range request to an inner HTTP/2 range request for the same immutable object. Home seeks to the requested offset and streams the bounded response. When the browser seeks again or disconnects, AWS cancels the obsolete inner HTTP/2 stream without closing unrelated streams or the outer WebSocket.

AWS may read ahead by a small bounded amount and cache complete files, sparse regions, or fixed cache blocks. Duplicate range requests should be coalesced. The complete immutable object retains its whole-file SHA-256 identity; cached regions require enough metadata to ensure they belong to the exact object and byte interval.

HTTP/2 concurrency and flow-control limits must prevent speculative video reads from exhausting AWS memory or starving small picture requests. A second authenticated home connection remains a fallback if the implementation cannot provide acceptable isolation over one connection.

### Segmented delivery option

HLS or DASH is a later option for adaptive playback. A home media worker would create a playlist and small immutable media segments. Each playlist or segment then fits the existing whole-object transfer and AWS cache model. Segmentation makes seeking and quality selection easier, but adds transcoding, playlist generation, more stored objects, and player compatibility work.

The preliminary sequence is therefore:

1. Support a web-optimized MP4 with public HTTP range requests.
2. Forward bounded ranges over inner HTTP/2 and add AWS caching.
3. Evaluate HLS/DASH when adaptive bitrate, multiple qualities, or longer videos justify it.

## 10. Originals and generated variants

The preliminary direction is:

- Home is authoritative for original media files.
- AWS is authoritative for posts, users, media metadata, and references to object IDs.
- AWS holds a bounded cache of public, immutable picture variants, video blocks, and later possibly video segments.
- Expensive image decoding, video inspection, and transcoding should not occur on the small AWS instance.

Media processing can run as a separate home process with access to the object store. The WebSocket transfer process should treat files as opaque bytes and should not parse, decompress, or transcode media. The interface between storage and media processing is still undecided.

Calling home storage a "cache" would be misleading if AWS deletes its staged copy after transfer. In that arrangement, home is primary object storage and must be backed up accordingly.

## 11. Content database and backup

### Database scope

SQLite is the initial database. The authoritative writable database resides on AWS and stores posts, publishing metadata, users and permissions, and references to immutable media object IDs. Picture and video bytes are never stored as SQLite BLOBs.

At the initial target of approximately 10,000 posts, a database containing text and object references is expected to remain in the hundreds-of-megabytes range. Revisions, comments, audit history, and full-text indexes may increase this substantially, so actual size and growth must be measured.

AWS is the only database writer. The home copy is a backup and must never be treated as an independently writable replica.

### Initial backup strategy

The first version deliberately transfers complete compressed snapshots instead of implementing incremental database synchronization:

1. At a scheduled low-traffic time, AWS uses SQLite's Online Backup API to create a consistent temporary snapshot without copying the live database file directly.
2. AWS closes the completed snapshot and calculates its uncompressed size and SHA-256 digest.
3. AWS compresses it with Zstandard using a conservative compression level and bounded resources.
4. AWS calculates the compressed size and digest and assigns a monotonically increasing backup generation.
5. AWS sends backup metadata and the compressed snapshot to home through a restricted inner HTTP/2 `PUT` stream.
6. Home writes it to a temporary file, validates its exact size and digest, flushes it to durable storage, and atomically publishes it.
7. Home acknowledges durable receipt. Only then may AWS delete the staged snapshot and compressed file.
8. A separate restore check periodically decompresses a backup, opens it with SQLite, runs an integrity check, and verifies expected schema/version metadata.

Database backups use a private backup namespace and are never addressable through public media URLs. The home server retains multiple generations so application mistakes, corruption, or unwanted changes are not immediately propagated into every usable backup.

The initial schedule and retention policy remain configurable. A reasonable prototype starting point is one daily snapshot with several daily and weekly generations retained. Backup transfer uses a low-priority, flow-controlled HTTP/2 stream so it does not intentionally compete with interactive media work.

### Later options

Incremental block synchronization or SQLite WAL replication may be added if measured database size, backup duration, or recovery-point requirements justify the extra machinery. Until then, complete compressed snapshots are easier to validate, restore, and reason about.

## 12. Failure behaviour

### Home unavailable

- Cached public pictures and cached video ranges or segments continue to work.
- Uncached media returns a controlled temporary-unavailable response or placeholder.
- AWS fails quickly rather than leaving public requests hanging.
- Uploads are either rejected clearly or staged within a bounded disk quota for later transfer.

### Connection interrupted

- The partial receiver file remains temporary and is deleted.
- An incomplete WebSocket message is never promoted to a stored object or cache entry.
- The home client reconnects with exponential backoff and jitter.
- AWS retries the entire idempotent `put` or `get`, subject to retry and time limits.
- The first version does not resume in the middle of a file.

### AWS restarted

- The web application, proxy, and file endpoint restart automatically.
- The home client reconnects.
- Durable AWS staging records are reconciled with staged files before retrying.
- In-memory queue entries alone must not be relied upon for acknowledged uploads.
- An unacknowledged database backup remains staged and may be retried idempotently by generation and digest.

### Storage full

- AWS evicts old or infrequently accessed cache entries within a configured limit.
- Home rejects new objects before exhausting the filesystem reserve.
- Neither side deletes authoritative originals automatically in response to disk pressure.
- Home does not remove the last known-good database backup to make room for a new generation.
- Disk usage is visible through simple status output and checked during routine maintenance.

## 13. Security model

The application-specific connection has a smaller privilege boundary than a reverse tunnel: compromising AWS does not directly provide a shell, arbitrary TCP access, or access to other home services. It can still abuse every permitted file operation, so the protocol and storage limits matter.

Controls for the first version:

- WSS with normal certificate and hostname validation.
- Dedicated, high-entropy HTTP Basic credentials checked during the WebSocket upgrade.
- A second proof-of-possession check using a dedicated Ed25519 client key and a fresh, expiring, single-use challenge.
- A pinned client-public-key registry on AWS, supporting key IDs, revocation, and overlapping keys during rotation.
- No application messages except `challenge`, `proof`, and authentication errors before the connection reaches the `ready` state.
- Independent rotation of the Basic password and Ed25519 key without embedding secrets in container images or source control.
- Optional mutual TLS as a future replacement for one or both client-authentication layers, not as file-operation authorization.
- Authentication messages are accepted only before the connection switches permanently into HTTP/2 byte-stream mode.
- Strict HTTP/2 limits for concurrent streams, headers, ranges, bodies, tasks, and buffers.
- Small bounded binary WebSocket messages inside the byte-stream adapter.
- Content-derived immutable object IDs; no caller-controlled paths.
- Exact length and SHA-256 validation before atomic promotion.
- Request, queue, transfer-time, retry, staging-disk, and home-storage quotas.
- An unprivileged home process restricted to its object directory.
- A read-only container filesystem except for the object and temporary-file volumes.
- No Docker socket, host networking, shell command execution, or arbitrary URL fetching.
- Logs that exclude credentials and unnecessary private metadata.
- Database-backup objects are private, excluded from public media routing, and readable only by the restricted backup/restore process.

A compromised AWS application could request known objects, upload unwanted data until its quota is reached, and deny service. Separate backups, quotas, monitoring, and conservative deletion rules limit the consequences.

## 14. Preliminary AWS resource budget

These are planning allowances, not measurements:

| Component | Approximate memory allowance |
|---|---:|
| Nginx | 20-50 MiB |
| Docker daemon, if used | 80-150 MiB |
| Operating system and services | 150-300 MiB |
| Journey application and WebSocket endpoint | To be measured |
| SQLite connections and bounded page cache | To be configured and measured |
| Snapshot compression | Temporary bounded memory and CPU; run off peak |
| Private transport working memory | Bounded per-stream HTTP/2 and WebSocket-adapter buffers |
| Public media cache | Primarily disk; bounded block/segment metadata index |
| Node.js frontend toolchain | Build time only; absent from production runtime |

The private transport should require little idle memory, but HTTP/2 multiplexing creates per-stream state and flow-control buffers. The byte-stream adapter therefore emits small bounded WebSocket messages rather than using one WebSocket message per object.

The first vertical slice must measure resident memory while idle, under a slow consumer, during concurrent streams, after cancellation, and during reconnect. Flow-control credit is released only as downstream consumers make progress.

Database settings, worker counts, unbounded queues, excessive video read-ahead, and accidental media decoding on AWS are likely to be larger memory risks than an idle connection.

## 15. Deployment direction

Docker Compose remains a candidate, but neither side requires every component to be containerized.

### AWS

- Nginx public reverse proxy serving compiled, fingerprinted React assets over HTTP/2
- Rust Journey API, including or adjacent to the WebSocket endpoint
- SQLite database on a persistent volume
- Scheduled consistent snapshot and Zstandard compression job
- Bounded staging area and public-media cache with byte-range support
- Console logs available through Docker for manual troubleshooting

### Home

- Journey home agent with the WebSocket adapter and inner HTTP/2 object server
- Mounted object-storage and temporary-file volumes
- Versioned compressed SQLite backup storage
- Optional separate image/video-processing worker
- Restore-validation, retention, and disk-health jobs

The home client may run in a small container with only the required data volume and outbound network access. Running directly under systemd is also reasonable and may be simpler. Containerization is isolation and packaging, not an authentication boundary by itself.

## 16. Alternatives and fallback options

### Reverse SSH tunnel

This reuses the already-open SSH service and mature tooling. It exposes a chosen home TCP service on an AWS-only port. It is suitable if the home side becomes a real HTTP service, but grants a more general transport capability and requires careful SSH key restrictions, bind addresses, forwarding rules, and process supervision.

### Chisel

Chisel provides a reverse TCP tunnel over an outbound WebSocket connection, with reconnection and multiplexing. It fits networks where WebSocket traversal is useful and allows the application to use ordinary HTTP. For the current two-operation file interface, it introduces a general tunnel that is not required.

### Tailscale

Tailscale provides a managed private network with machine identity, access policies, NAT traversal, and encrypted relay fallback. It is operationally convenient when several services or machines must communicate, but it is broader than the current file-transfer requirement and depends on a coordination service.

### Plain WireGuard

WireGuard offers a small encrypted network tunnel without a hosted coordination dependency. It requires manual key, routing, endpoint, firewall, and keepalive management. AWS would normally be the fixed public UDP endpoint.

### External object storage

S3 or another object store could hold originals or public variants. This would improve availability and remove the live home dependency, at the cost of another service and storing picture copies outside home.

## 17. Important open questions

### Product

- Is Journey a private hobby, a public personal site, or a commercial service?
- Who can author content, and how many authors are expected?
- Are comments, themes, plugins, revisions, scheduling, and moderation required?
- What does "like WordPress" mean for the first usable release?
- Which public routes should Rust render as indexable HTML, and which should be handled entirely by React?
- What editing component and content format should the React authoring interface use?

### Content and metadata

- Must authors be able to work while the home server is offline?
- How long may an upload remain staged on AWS?
- How many post revisions, comments, and audit records will be retained?
- What daily/weekly database-backup retention policy is required?
- What recovery-point and recovery-time objectives are acceptable?

### Media and protocol limits

- Expected number, average size, and maximum size of picture and video originals?
- What bounded WebSocket payload size gives the byte-stream adapter good throughput without excessive buffering?
- What maximum number of inner HTTP/2 streams should be allowed initially?
- Must originals remain exclusively at home after successful transfer?
- Which generated sizes and formats should AWS cache?
- Where and how are variants generated and registered?
- How much AWS disk may staging and cache data consume?
- Are private pictures or per-user access controls required?
- Should the production WebSocket byte-stream adapter use an existing crate or a small project-owned implementation?

### Video

- What maximum duration, resolution, codec, and file size must be supported initially?
- Is a web-optimized MP4 sufficient initially, or is adaptive HLS/DASH required?
- Which browsers and devices must play uploaded videos?
- What maximum forwarded range and read-ahead size gives a good balance between startup latency, throughput, and cache efficiency?
- How much video data may AWS read ahead and cache per active viewer?
- At what traffic level should video receive a separate authenticated home connection?

### Availability and performance

- What is the home connection's sustained upload speed?
- How often is the home server offline?
- What should visitors see when an uncached object is unavailable?
- How many simultaneous visitors and cache misses are expected?
- How much bounded concurrency can the AWS micro instance and home uplink support?

### Operations

- Which Linux distributions run on AWS and at home?
- Is Docker already installed on either machine?
- How will Basic credentials and Ed25519 client keys be provisioned, backed up, revoked, and rotated?
- How will logs, metrics, updates, backups, and alerts be managed?
- What is the recovery process if the home disks fail?

## 18. Suggested prototype milestones

The internal-network transport proof of concept is specified separately in [Vertical Slice 01: HTTP/2 over WebSocket Object Download](vertical-slice-01-http2-over-websocket.md). WebSocket is already selected as the outer transport; the protocol carried inside it remains the important decision. The slice therefore first exercises the role-reversed HTTP/2 object session over a bounded `tokio::io::duplex` harness, then inserts a WebSocket bridge without changing the HTTP/2 or object-service code. It validates streaming, ranges, multiplexing, cancellation, and backpressure before the broader architecture proceeds through these milestones:

1. **Secure connection:** Upgrade the proven connection to WSS with HTTP Basic followed by the Ed25519 challenge, confirm that pre-authentication file messages are rejected, and exercise heartbeat/reconnect behaviour.
2. **Safe storage:** Add idempotent inner HTTP/2 `PUT`, temporary files, size/digest checks, atomic rename, and storage quota.
3. **Failure tests:** Interrupt uploads, restart both endpoints, send invalid requests and excessive streams, and fill staging areas safely.
4. **AWS cache:** Add bounded atomic caching, request coalescing, queue limits, and offline behaviour.
5. **Publishing slice:** Build React in a disposable Node.js container stage, serve the compiled assets from Nginx, load cursor-paginated post data from Rust, and publish one post with pictures over public HTTP/2.
6. **Database backup:** Generate, compress, transfer, retain, restore, and integrity-check a complete SQLite snapshot while the application remains active.
7. **Video slice:** Integrate browser playback with the already-proven inner HTTP/2 range path and test seeking and cache reuse.
8. **Load measurement:** Measure AWS/home RAM, CPU, bandwidth, snapshot/compression cost, video startup time, seek latency, maximum-file behavior, and queue delay.
9. **Architecture review:** Decide whether the adapter and inner HTTP/2 remain appropriate, video needs a second connection, HLS/DASH is justified, database backups need incremental replication, or a standard tunnel/object store is now preferable.

## 19. Current preliminary decisions

| Topic | Current direction | Confidence |
|---|---|---|
| Public entry point | Small AWS instance | High |
| Public web protocol | HTTPS with HTTP/2 preferred and HTTP/1.1 fallback | High |
| Browser interface | Precompiled React assets with dynamic Rust JSON APIs | High |
| Infinite scrolling | Cursor pagination loaded on demand by React | Medium |
| Frontend build | Pinned Node.js multi-stage container build | High |
| Production Node.js service | None | High |
| Website logic and authoritative database | AWS | High |
| Initial database | SQLite; media bytes excluded | Medium |
| Database backup | Complete consistent Zstandard-compressed snapshots sent home | Medium |
| Database replica writes | AWS only; home backups are read-only | High |
| Original picture storage | Home server, with backups | High |
| Home inbound exposure | None | High |
| Data connection | One home-initiated WSS connection | Medium |
| Connection authentication | HTTPS + HTTP Basic + Ed25519 challenge-response | Medium |
| Private transfer protocol | Restricted HTTP/2 object API over a WebSocket byte stream | Medium |
| Concurrency | Small bounded number of HTTP/2 streams | Medium |
| Object identity | Immutable SHA-256-derived IDs | Medium |
| Image processing | Separate home process | Medium |
| Public image performance | Bounded AWS cache | Medium |
| Initial public video delivery | Web-optimized MP4 with HTTP byte ranges | Medium |
| Home-to-AWS video retrieval | HTTP byte ranges over inner HTTP/2 | Medium |
| Adaptive video | Evaluate HLS/DASH after basic MP4 streaming | Low |
| Deployment | Docker Compose on both machines | Medium |
| Backend | Rust; framework undecided | Medium |
| Public rendering and editor details | Undecided | Low |

## 20. References

- [The WebSocket Protocol, RFC 6455](https://www.rfc-editor.org/rfc/rfc6455)
- [HTTP/2, RFC 9113](https://www.rfc-editor.org/rfc/rfc9113)
- [HTTP Semantics: Range Requests, RFC 9110](https://www.rfc-editor.org/rfc/rfc9110#name-range-requests)
- [HTTP Live Streaming, RFC 8216](https://www.rfc-editor.org/rfc/rfc8216)
- [SQLite Online Backup API](https://www.sqlite.org/backup.html)
- [Zstandard repository and documentation](https://github.com/facebook/zstd)
- [Nginx WebSocket proxying](https://nginx.org/en/docs/http/websocket.html)
- [Docker memory constraints](https://docs.docker.com/engine/containers/resource_constraints/)
- [Docker multi-stage builds](https://docs.docker.com/build/building/multi-stage/)
- [Chisel repository and documentation](https://github.com/jpillora/chisel)
- [Tailscale control and data planes](https://tailscale.com/docs/concepts/control-data-planes)
- [WireGuard quick start](https://www.wireguard.com/quickstart/)
