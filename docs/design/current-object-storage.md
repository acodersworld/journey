# Current Object Storage Design

**Status:** Implemented in `journey-storage`  
**Scope:** Storage interface, HTTP/2 and browser adapters, and both backends

The Rust interface and code are authoritative if this description becomes
stale. The older [SQLite storage design](home-object-storage-design.md) and
[interface proposal](home-object-storage-interface.md) record historical
decisions, not the current implementation. The filesystem backend is available
to the h2c example; the separate home application has not adopted it yet.

## Interface and HTTP routes

`StoreInterface` provides GET, STAT, prefix LIST, idempotent DELETE, and
streaming PUT. The HTTP/2 adapter exposes `GET`, `HEAD`, `PUT`, and `DELETE` at
`/objects/<key>` and LIST at `/objects?prefix=<prefix>&limit=<n>&cursor=<token>`.
`PUT /objects` also accepts a streamed upload when it has exactly one
`Object-Key-Mode: sha256` header and no query string. Its logical key is the
lowercase, 64-character SHA-256 digest of the complete payload, including for
an empty payload; a missing or repeated mode header, an unsupported mode, and
a query string are rejected with `400`. Both PUT routes require `Content-Type`
and support unconditional, create-only (`If-None-Match: *`), and replace-only
(`If-Match: *`) publication. Conditions are checked against the supplied or
derived key at publication time. Successful PUT responses are `200` with an
empty body and the full stored key in `Object-Key`; they do not include
`Location`. Generated PUTs also return the generated suffix as
`Object-Name: <name>`.

`PUT /objects/<prefix>/` accepts the same generated mode and stores the
lowercase payload digest after the decoded prefix. The returned `Object-Name`
contains only the 64-character digest; combine it with the request prefix to
form the full stored key returned in `Object-Key`. At the root, the name and
key contain the same digest. A nonempty prefix must end in `/`, and its UTF-8
byte length plus 64 must not exceed 1,024. A generated-mode header on a keyed
path without a trailing slash is invalid. `PUT /objects/` remains invalid.
Logical object keys cannot end in `/`; that suffix is reserved for virtual
folders, so GET, HEAD, and DELETE reject such keys as well.

The Rust PUT interface separates these publication modes by type. A caller
supplies a full `Key` to `put_context_with_key` and commits its context with
`put_with_key`, which returns `()`. A caller supplies a prefix to
`put_context_with_generated_name` and commits its distinct context with
`put_with_generated_name`, which returns an `ObjectName`. `ObjectName` contains
only the lowercase 64-character payload digest and implements `as_str()` and
`Display`; its constructor validates that representation. The adapter combines
it with the known prefix when it needs the full stored `Key`. Contexts
implement `PutContextInterface` for streaming appends, but each context can
only be committed by its matching operation.

The adapter decodes object-path percent escapes exactly once as UTF-8. LIST
decodes its query values once and returns logical keys in JSON; its unpadded
Base64URL cursor identifies the inclusive next key. GET can select one byte
range. The same interface has an in-memory backend.

The separate browser object manager and its authenticated HTTP routes are
described in [Storage Web Interface](storage-web-interface.md); its JSON upload
response continues to return the full generated key.

## On-demand video thumbnails

`GET` and `HEAD /objects/<key>` accept one optional
`Object-Representation: thumbnail` header. Without it, the original object is
returned. The thumbnail uses the same logical key and is available only when
the stored content type is exactly `video/mp4` or `video/quicktime` (case
insensitive). Thumbnail requests reject `Range`; unknown or duplicate
representation headers return `400`, and unsupported object types return
`415`. Original and thumbnail responses include
`Vary: Object-Representation`. Thumbnail `HEAD` requests generate or read the
same cached image as `GET` to return its exact content length.

`storage.thumbnail_time_ms` selects the first decodable video frame at or after
the configured time, and defaults to zero. If that time exceeds the video
duration, the first decodable frame is used. Requests cannot select another
time. The filesystem store gives FFmpeg a seekable payload-only reader over
the immutable object's `Arc<File>`; positional reads start after the object
metadata and stop at the declared payload length. Decoding and JPEG encoding
run in blocking tasks, with at most two decodes at once. The output preserves
the frame's aspect ratio and has a longest edge no larger than 640 pixels.

Generated JPEGs are stored under `<object_dir>/thumbnail-cache`, beside
`objects/` and `part/`. This directory is outside the indexed object namespace;
logical keys still map to hashed `.obj` files under `objects/`. Cache filenames
hash the source object's UUID and thumbnail settings, so replacing a logical
key cannot reuse an older object's image. Cache writes use a synced temporary
file and atomic rename. Requests for one cache entry share an in-process
generation lock; missing, malformed, or undecodable cache files are rebuilt.
The cache is persistent across service restarts and can be deleted without
affecting stored objects. This representation is private to the storage object
service; it adds no public thumbnail URL or website behavior.

Logical keys are nonempty UTF-8 of at most 1,024 bytes and cannot end in `/`.
Content types contain 1 through 128 header bytes. The filesystem backend maps
a key to the lowercase SHA-256 digest of its UTF-8 bytes followed by `.obj`;
client keys never become filesystem path components.

## Published file format

Each immutable file in `objects/` consists of metadata followed immediately by
the original payload. Version 1 uses little-endian integers and this layout:

| Offset | Bytes | Field |
| ---: | ---: | --- |
| 0 | 2 | Format version, `1` |
| 2 | 8 | Magic bytes `OBJSTORE` |
| 10 | 2 | Total metadata length, `u16` |
| 12 | 4 | Metadata CRC-32/ISO-HDLC |
| 16 | 8 | Payload length, `u64` |
| 24 | 8 | Creation time in Unix milliseconds, `u64` |
| 32 | 32 | Raw payload SHA-256 |
| 64 | 16 | Raw version 4 object UUID, renewed on every PUT |
| 80 | 2 | UTF-8 key length, `u16` |
| 82 | 2 | Content-type length, `u16` |
| 84 | Variable | Exact key bytes, then exact content-type header bytes |

There is no padding. `metadata_len = 84 + key_len + content_type_len`, with
accepted bounds of 86 through 1,236 bytes. The CRC covers all metadata bytes
with bytes 12–15 zeroed; it excludes the payload. The version is parsed before
any later field because future versions may change their layout. A valid file
has `file_len = metadata_len + payload_len`.

## Publication and recovery

The configured root contains `objects/`, `part/`, and an abnormal-event journal
that defaults to `journal`. The store holds an exclusive root lock. On startup
it clears leftover unpublished files from `part/`, then scans `objects/` and
rebuilds a `BTreeMap` keyed by logical key. It validates the header, metadata
CRC, embedded key against the derived filename, and file length without
reading or hashing payload bytes. Healthy entries retain metadata and an
`Arc<File>`; entries with a recoverable key can instead be marked corrupt.
Files without a trustworthy key are skipped and reported by physical filename.
Existing files whose embedded key ends in `/` are left untouched, skipped from
the index, and reported to stderr and the abnormal-event journal.
Corrupt entries are omitted from LIST and return a corruption error on GET or
HEAD. The [deferred diagnostics plan](../deferred/storage-diagnostics-and-administration.md)
covers explicit payload SHA-256 checks, manifest export, and diagnostic web views.

PUT writes a randomly named, exclusively created `.part` file, hashing and
counting the payload as it streams. It fills in metadata, syncs the file, then
serializes the condition check, rename into `objects/`, and index update.
For `PUT /objects` and `PUT /objects/<prefix>/`, the incremental payload digest
becomes the final 64 characters of the logical key before metadata is
finalized. The part file is published directly at that key's derived physical
path without publishing or renaming a temporary logical object. Prefixes are
virtual and do not change the filesystem layout. The condition check, rename
into `objects/`, and index update are serialized, so concurrent create-only
uploads for the same generated key have one winner. An unconditional or
replace-only PUT can repair a corrupt entry at the selected key; an unindexed
file occupying the derived path is preserved instead of overwritten. Failed
or cancelled uploads are removed when possible, with startup cleanup handling
leftovers. Part files and published files must share a filesystem so rename is
atomic.

GET and HEAD copy an index entry under a read lock and release that lock before
file I/O. The reader retains the entry's `Arc<File>` and uses positional Unix
`FileExt::read_at`, so concurrent reads have independent cursors and a read
already in progress can finish after replacement or deletion. Normal GET does
not hash the payload. DELETE removes the derived file and index entry, even for
a corrupt entry, and is idempotent.

The store does not sync `part/` or `objects/` directories after publication or
deletion. A crash or power loss in the small window before a directory change
reaches storage can lose an acknowledged PUT or make a deleted file reappear.
This durability tradeoff is accepted for this store.

## Abnormal-event journal

Startup corruption, access to keyed corrupt entries, and deletion of keyed
corrupt or unindexed files are written to stderr and, best effort, to a JSON
Lines journal. Each event includes a UTC `time`, `event`, `reason`, and the
logical `key` or physical `file` when available; a validated UUID appears as
`object_id`. Journal failure does not turn an otherwise successful operation
into a failure. The path is configurable. Host `logrotate` can rotate at 10 MiB
and retain five numbered files; the store opens the current path for each
append, so rotation should use rename rather than `copytruncate`.
