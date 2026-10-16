# Website image disk cache

The optional site image cache stores complete image responses on the site host.
It is disabled by default. When enabled, it caches quality JPEGs for displayed
images, original images requested for download, video
thumbnails, and the reduced JPEG images used by WhatsApp previews. Raw video
playback, byte ranges, partial responses, errors, and images over the configured
entry limit continue to stream directly from storage.

## Configuration and storage

The `[image_cache]` section is shown in `apps/site/config.example.toml`. Byte
limits accept positive TOML integers or size strings using the site's existing
size suffixes (`B`, `K`, `KiB`, `KB`, `M`, `MiB`, `MB`, `G`, `GiB`, and `GB`,
case insensitive). Eviction and access timestamp intervals use a positive
integer followed by `s`, `m`, `h`, or `d`. The default cache directory is
`image-cache/` beside the configured database. An explicit relative directory
is resolved from the configuration file's directory.

Mount the cache directory on persistent storage when caching is enabled. The
`cache/` and `parts/` directories are created below the configured root and
must remain on the same filesystem so completed files can be published
atomically. Both directories and cache files use private permissions. The
mounted cache root must have mode `0700` and be writable by the site process;
the deployment container runs as UID and GID `65532`. Set host bind mount
ownership to `65532:65532` and mode `0700`. The existing `site-data` volume can
hold the default cache when the database is stored there.

Entries contain versioned identity metadata and the image payload in one file.
The entry filename is a SHA-256 digest of its storage key, representation, and
reduction options. Quality JPEG entries include the configured quality value,
so changing it cannot reuse an original or a JPEG at another quality. Existing
cache files remain readable. Cache contents are populated on request. There is no TTL or
prefetching. Site media keys are assumed to identify immutable bytes; after
manually replacing content under an existing key, or changing derivative
generation behavior, clear the affected entries so old bytes are not reused.

## Capacity and eviction

The cache reserves the declared payload length before creating a temporary
part. A request streams upstream bytes to the viewer while writing the part.
Only a complete `200` image with a valid content type, positive declared
length, and exact final byte count is published. Concurrent misses fill
independently. The first completed entry wins; duplicate parts are discarded.
If space or a fill slot is unavailable, the request streams without caching.

Periodic eviction scans completed entries and refreshes the recent-post set
from published posts ordered by publication time and ID, descending. Entries
for the latest `recent_post_count` posts receive retention priority by storage
key, including every cached representation of each key. Eviction still removes
recent entries when required. Within each priority group, the oldest file
modification time is evicted first. Cache hits update that timestamp only
after `access_timestamp_interval` has elapsed.

One `journey-site` process may use a cache directory at a time. Startup removes
abandoned parts, retains completed entries, scans occupancy, and evicts if
occupancy exceeds the configured capacity. Filesystem and cache write failures
are logged and the upstream response continues wherever possible.

## Clearing entries

Use the private control socket while the site is running:

```sh
journey-site image-cache clear
journey-site image-cache clear --key media/<storage-key>
```

The first command removes every completed entry. The keyed command removes all
representations for that storage key. A fill that began before a clear cannot
publish its part after the clear completes.
