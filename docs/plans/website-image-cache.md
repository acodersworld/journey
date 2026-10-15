# Website image disk cache

## Summary

Add a persistent disk cache to the website to reduce downloads from home storage. Cache original images, requested reduced images, and video thumbnails. Video playback bypasses the cache.

Use direct filesystem lookups, periodic LRU eviction, and higher retention priority for recent published posts. Populate entries only when requested.

## Configuration and layout

Add an optional configuration section:

```toml
[image_cache]
enabled = false
# directory = "./image-cache"
max_bytes = "1GiB"
max_entry_bytes = "64MiB"
max_concurrent_fills = 4
recent_post_count = 5
eviction_interval = "1h"
eviction_target_percent = 90
access_timestamp_interval = "1m"
```

- Default directory: `image-cache/` beside the site database. Explicit relative paths resolve against the configuration file's directory.
- Existing deployments retain their behavior until caching is enabled.
- Permit `recent_post_count = 0` for ordinary LRU.

### Size and duration values

Both byte settings accept integer byte counts or strings using the existing performance size parser's case-insensitive conventions:

| Suffix | Meaning |
|---|---|
| None or `B` | Bytes |
| `K`, `M`, `G` | Binary multiples of 1024 |
| `KiB`, `MiB`, `GiB` | Binary multiples of 1024 |
| `KB`, `MB`, `GB` | Decimal multiples of 1000 |

Both interval settings require strings containing a positive whole number followed by a case-insensitive `s`, `m`, `h`, or `d` suffix. Suffixes are mandatory: reject integer TOML values and strings containing only digits. Defaults remain one hour (`"1h"`) and one minute (`"1m"`).

Reject unsupported suffixes, fractional or nonpositive values, and overflow. Require positive concurrency, an eviction target between 1 and 99 percent, and an entry limit no greater than total capacity.

### Directory structure

```text
image-cache/                 mount this directory
├── cache/                  completed entries
└── parts/                  temporary downloads
```

Both subdirectories must share a filesystem for atomic publication. Use private directory permissions and document persistent mounting with appropriate ownership.

Store versioned metadata and image payload together in each entry. Metadata contains the full cache identity, content type, and payload length. Derive filenames from a hash of the identity.

## Request handling

### Authorization and lookup

- Perform existing account, draft, share-link, and WhatsApp capability checks before every lookup.
- Identify entries by storage key, representation, and all reduction options. Authorized requests may share cached bytes.
- Open the derived cache filename directly. Maintain no in-memory index of completed entries.
- Validate metadata, identity, and file length when opening an entry. Remove invalid entries and fetch from storage.
- Stream hits from disk using bounded buffers.
- Preserve existing response headers, guest `no-store` policies, and request-specific download filenames.
- Cached HEAD responses use entry metadata. HEAD misses use upstream HEAD without populating the cache.

Integrate caching into site media handling after authorization. Performance diagnostics and database backups continue using storage directly.

### Admission and streaming

Admit complete `200` image responses with a valid content type and known positive length. Raw video, partial responses, errors, and oversized images bypass caching.

```text
storage → response stream → viewer
                 │
                 └── write parts/<unique-name>
                              │
                       validate completion
                              │
                       atomic publication
                              ▼
                         cache/<identity>
```

- Each admitted request streams to its viewer while writing its own unique part file.
- Reserve the declared size before creating the part. Track completed bytes and active reservations in small counters.
- If capacity or a fill slot is unavailable, stream normally without caching. Requests do not trigger eviction or wait for another fill.
- Publish only after successful upstream completion and an exact payload-length check.
- Cancellation or upstream failure closes and deletes the part. No detached task continues downloading.
- Concurrent misses may download independently. Publish without replacing an existing completed entry; discard duplicate parts and count the completed entry once.
- Cache write failures produce warnings and abandon caching while preserving upstream delivery wherever possible.
- Use asynchronous filesystem operations or bounded blocking work. Keep filesystem and network operations outside bookkeeping locks.

## Eviction and administration

### Periodic maintenance

Run eviction every hour by default:

1. Scan completed files and their metadata.
2. Determine recent-post membership.
3. If occupancy exceeds the target, evict toward 90% of configured capacity.
4. Evict ordinary entries first, then recent entries if necessary, using oldest access time within each class.

Recent means referenced by the latest five published posts, ordered by publication time descending and then ID descending. Apply priority to all cached representations of those storage keys. Refresh this classification for every eviction sweep.

Recent entries have retention priority and remain evictable. Do not prefetch images or apply a TTL.

Use each completed file's modification timestamp as its last-access marker. On a hit, update it only if at least `access_timestamp_interval` has elapsed—one minute by default. This changes filesystem metadata without rewriting the image payload.

Coordinate scans, publication, and clearing so concurrent operations preserve correct accounting. Active parts occupy reserved space and cannot be evicted; a sweep stops if no completed entries remain available to reclaim.

### Startup and clearing

- At startup, clear abandoned parts and scan completed files to establish occupancy.
- Retain completed entries across restarts. Perform detailed entry validation when opened.
- If startup occupancy exceeds the configured limit, perform an initial eviction pass.
- Support one running site process per cache directory.
- Follow existing graceful shutdown behavior and clean up interrupted parts.

Add private control-socket commands:

```text
journey-site image-cache clear
journey-site image-cache clear --key <STORAGE_KEY>
```

Keyed clearing removes all representations of that storage key. Prevent requests started before clearing from publishing affected entries afterward.

### Integration and operations

Keep implementation and tests in dedicated cache modules, with targeted wiring into configuration, media handling, startup, and the control socket.

Use existing logging: DEBUG for cache activity, WARN for failures, and INFO for startup occupancy, eviction summaries, and explicit clearing.

Document configuration, persistent mounts, and cache clearing. Assume site media keys identify immutable content; document clearing after manually replacing content under an existing key or changing derivative generation behavior.

No database migration, storage protocol change, or image-resizing behavior change is required.

## Verification

- Configuration accepts integer bytes and supported size suffixes; defaults retain their intended values.
- Both interval settings accept supported case-insensitive duration suffixes and reject bare integers and suffixless strings.
- Invalid suffixes, fractional values, zero, negative values, overflow, and inconsistent limits are rejected.
- Cache hits avoid upstream downloads; video playback, partial responses, and oversized entries bypass caching.
- Original images, reduced variants, and video thumbnails have distinct identities.
- Streaming begins before completion; cancellation, truncation, and upstream failures never publish incomplete entries.
- Concurrent duplicate fills publish one entry and release temporary reservations.
- Capacity limits and busy-fill bypass work during concurrent requests.
- Periodic eviction respects access ordering, recent-post priority, and the configured target.
- Timestamp updates are throttled; ordinary requests perform no directory scans.
- Restart preserves completed entries, clears abandoned parts, and rebuilds occupancy correctly.
- Corrupt entries and cache I/O failures fall back safely.
- Expired, revoked, and unauthorized access remains denied when bytes are cached.
- HEAD behavior, download headers, WhatsApp limits, and guest response policies remain correct.
- Clearing during an active fill prevents that fill from republishing the cleared entry.
