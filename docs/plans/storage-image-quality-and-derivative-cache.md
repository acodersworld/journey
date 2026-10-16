# Storage image quality and derivative cache

## Summary

Add JPEG re-encoding at original display dimensions, selected by an encoding-quality header. Add a content-and-settings-addressed disk cache shared by re-encoded images, reduced images, and video thumbnails.

Use existing `ffmpeg-next` for processing and filesystem APIs for caching. This change covers the storage crate, storage application, example server, and documentation. Website integration is separate.

## HTTP behavior

### Quality-only requests

```http
GET /objects/<key>
Object-Image-Quality: 80
Object-Image-Cache: true
```

- `Object-Image-Quality` selects JPEG re-encoding and accepts an integer from 1 through 100.
- Preserve full image resolution after applying EXIF orientation. Do not resize or impose the existing 2048-pixel output limit on this mode.
- Retain existing source-byte and decoded-pixel safety limits.
- Composite transparency onto white. Animated inputs produce their first frame, consistent with existing image reduction.
- Reject combinations with `Object-Representation`, existing image dimension/fit/format/byte-limit headers, or `Range`.
- Reject video inputs. Leave the original stored object unchanged.
- Quality controls compression; it does not guarantee a smaller file or a particular byte size.

Translate quality into FFmpeg's existing MJPEG quantizer scale using:

```text
quantizer = round(2 + 29 × (100 − quality) / 99)
```

Document that this is the service's quality scale and does not exactly match other JPEG encoders. Keep existing reduced-image encoding behavior unchanged.

### Cache flag

Accept `Object-Image-Cache: true|false` on quality-only requests and existing reduced-image and thumbnail requests.

| Setting | Behavior |
|---|---|
| `true` | Read existing entries; generate and attempt to cache misses |
| `false` | Read existing entries; generate misses without caching |
| Omitted on quality/reduced-image requests | Same as `false` |
| Omitted on thumbnail requests | Same as `true`, preserving existing behavior |

- `false` prevents populating both final results and intermediate thumbnail entries. Hits still update throttled access timestamps.
- Reject duplicate or invalid values and use on original-object requests without a transformation.
- Support GET and HEAD. HEAD generates or retrieves the same representation and returns its headers without a body.
- Include both new headers in `Vary`.
- Preserve existing authentication, error mappings, and generated-response content headers.

## Storage interfaces and cache identity

Add validated quality and derivative-request types, plus a cache policy distinguishing read-only admission from read/write admission.

Provide an additive `get_image_derivative` storage method covering quality-only images, reduced images, and thumbnails with optional reduction. Give it a default implementation so existing storage implementations continue compiling. Preserve existing public method signatures and behavior.

The filesystem store implements disk caching. Other implementations may generate without retaining results; cache admission is best effort.

For filesystem objects:

- Retain the existing stored payload SHA-256 in the private in-memory index during loading and PUT publication. No object-file format change is required.
- Resolve the source to one immutable object snapshot before checking the cache. Generation must use that same snapshot.
- Apply existing source existence, corruption, content-type, and length checks before serving cached data.
- Do not rehash payloads on cache lookups.

Use a canonical identity containing:

```text
source payload SHA-256
+ relevant source content type
+ transformation kind and effective settings
+ transformation implementation version
```

Settings include quality, resolved output format, dimensions, fit, byte cap, and thumbnail extraction time where applicable. Normalize defaults. Exclude logical keys, object UUIDs, and the cache flag.

Identical content under different keys can share entries. Replacing content changes its identity automatically. Include a version discriminator so changed encoder behavior cannot reuse incompatible results.

## Disk cache and lifecycle

Add configuration to the storage application and corresponding filesystem-store settings:

```toml
[derivative_cache]
enabled = true
max_bytes = "1GiB"
max_entry_bytes = "64MiB"
eviction_interval = "1h"
eviction_target_percent = 90
access_timestamp_interval = "1m"
```

Use the same size conventions as the website cache. Require duration strings with case-insensitive `s`, `m`, `h`, or `d` suffixes; reject bare durations. Validate positive limits and intervals, overflow, entry limits exceeding total capacity, and eviction targets outside 1–99.

Use an internal directory beneath the filesystem-store root:

```text
derivative-cache/
├── cache/
└── parts/
```

- This directory is outside the logical object namespace and stays on the store's mounted filesystem.
- Store versioned identity metadata, content type, length, and payload together. Publish unique parts atomically.
- Open entries directly by hashed identity; maintain no in-memory entry index.
- Validate entry metadata and file length on reads. Invalid entries become misses.
- Use small occupancy counters and reservations to bound cache admission. Full or oversized results are returned without caching.
- Concurrent misses may generate independently. The first publisher wins; discard duplicates without double-counting.
- Perform CPU and filesystem work off async executor threads. Preserve the existing shared limit of two concurrent image-processing jobs without acquiring that permit recursively.
- Cache failures log warnings and return the generated result wherever possible.

Run timestamp-based LRU eviction hourly by default, reclaiming toward 90% capacity. Update modification timestamps on hits at most once per minute. Use no TTL, prefetching, recent-post priority, or detached fill tasks.

At startup, remove abandoned parts, calculate occupancy, and enforce capacity when exceeded. Manage maintenance task shutdown with store ownership.

Replace the existing UUID-addressed thumbnail cache with this shared cache. After successful initialization, remove only recognized legacy thumbnail cache files; regenerate thumbnails on demand. Preserve originals and unknown files. Existing deployments need no database or object-format migration.

Keep cache and encoding implementation in dedicated modules. Wire configuration into the storage application and example server, update deployment examples, and document that cached derivatives are disposable.

## Verification

- Quality-only requests preserve resolution, apply orientation, produce JPEG, and support images above 2048 pixels within existing safety limits.
- Quality endpoints and intermediate values map correctly; invalid quality, conflicting headers, duplicate headers, video inputs, and ranges are rejected.
- Existing reduction and thumbnail requests retain their output behavior.
- `true` populates misses; `false` reads hits without populating misses or intermediate entries. Omitted flags preserve the specified defaults.
- Identical payloads under different keys share entries; changed content and settings use distinct entries.
- Missing or corrupt underlying objects cannot be served through cached derivatives.
- Concurrent replacement uses one source snapshot throughout lookup and generation.
- Cache publication, duplicate races, capacity accounting, corrupt entries, I/O failures, restart cleanup, and LRU eviction behave correctly.
- Existing thumbnail cache cleanup preserves original objects and unknown files.
- GET/HEAD headers, `Vary`, configuration suffix parsing, legacy configuration defaults, and maintenance shutdown are covered.
