# Current Object Storage Design

**Status:** Implemented in `journey-storage`  
**Scope:** Storage interface, HTTP/2 adapter, and filesystem backend

The Rust interface and code are authoritative if this description becomes
stale. The older [SQLite storage design](home-object-storage-design.md) and
[interface proposal](home-object-storage-interface.md) record historical
decisions, not the current implementation. The filesystem backend is available
to the h2c example; the separate home application has not adopted it yet.

## Interface and HTTP routes

`StoreInterface` provides GET, STAT, prefix LIST, idempotent DELETE, and
streaming PUT. The HTTP/2 adapter exposes `GET`, `HEAD`, `PUT`, and `DELETE` at
`/objects/<key>` and LIST at `/objects?prefix=<prefix>&limit=<n>&cursor=<token>`.
It decodes object-path percent escapes exactly once as UTF-8. LIST decodes its
query values once and returns logical keys in JSON; its unpadded Base64URL
cursor identifies the inclusive next key. PUT supports unconditional,
create-only (`If-None-Match: *`), and replace-only (`If-Match: *`) publication.
GET can select one byte range. The same interface has an in-memory backend.

The separate browser object manager and its authenticated HTTP routes are
described in [Storage Web Interface](storage-web-interface.md).

Logical keys are nonempty UTF-8 of at most 1,024 bytes. Content types contain
1 through 128 header bytes. The filesystem backend maps a key to the lowercase
SHA-256 digest of its UTF-8 bytes followed by `.obj`; client keys never become
filesystem path components.

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
Corrupt entries are omitted from LIST and return a corruption error on GET or
HEAD. The [deferred diagnostics plan](../plans/storage-diagnostics-and-administration-deferred.md)
covers explicit payload SHA-256 checks, manifest export, and diagnostic web views.

PUT writes a randomly named, exclusively created `.part` file, hashing and
counting the payload as it streams. It fills in metadata, syncs the file, then
serializes the condition check, rename into `objects/`, and index update.
Create-only and replace-only conditions are evaluated at publication time.
An unconditional or replace-only PUT can repair a keyed corrupt entry; an
unindexed file occupying the derived path is preserved instead of overwritten.
Failed or cancelled uploads are removed when possible, with startup cleanup
handling leftovers. Part files and published files must share a filesystem so
rename is atomic.

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
