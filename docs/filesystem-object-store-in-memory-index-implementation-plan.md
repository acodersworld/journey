# Filesystem Object Store with an In-Memory Index

**Status:** Ready for implementation; async reader implemented  
**Created:** 26 September 2026  
**Scope:** Replace the preliminary SQLite index design with a startup-built
`BTreeMap` while retaining self-describing object files

## 1. Goal and authority

Implement the existing `journey-storage` object interface on local files with
bounded streaming uploads and range reads. Published object files are the only
durable record of object identity, metadata, and payload. There is no SQLite
database or other live on-disk index. At startup, scan published files and
build an ordered in-memory `BTreeMap` for exact lookup and prefix LIST.

This document replaces the SQLite indexing proposal in
[Home Object Storage Design](home-object-storage-design.md) for the filesystem
implementation. It preserves that document's flat, server-derived filenames,
immutable published containers, atomic publication, and storage-root
confinement. The existing HTTP/2 object routes and response formats remain the
client-facing contract. Where older design and interface documents still
describe SQLite, directory sync, or other superseded choices, this plan is
authoritative. Integrity verification, manifest export, and the LAN admin
page are deferred to
[Storage Diagnostics and Administration](storage-diagnostics-and-administration-deferred.md).

## 2. Published file format

Each `.obj` file contains variable-length metadata followed immediately by
the original payload:

```text
fixed preamble | remaining metadata | payload bytes
<-------- metadata_len -------->|<-------- payload_len -------->
```

The first two bytes are always a little-endian `u16` format version. Parse
that version before interpreting any later bytes; all fields after it,
including the metadata-length encoding, belong to that version's layout.
Version 1 has this exact layout, with all integers little-endian:

| Offset | Size | Version 1 field |
| ---: | ---: | --- |
| 0 | 2 | Format version, `1` |
| 2 | 8 | Fixed magic bytes `OBJSTORE`, with no terminator |
| 10 | 2 | `u16 metadata_len`, the total metadata length |
| 12 | 4 | Metadata CRC-32/ISO-HDLC |
| 16 | 8 | `u64 payload_len` |
| 24 | 8 | `u64 created_at_ms`, Unix milliseconds |
| 32 | 32 | Raw SHA-256 digest of the payload |
| 64 | 16 | Raw version 4 object UUID, regenerated for each PUT |
| 80 | 2 | `u16 key_len` in bytes |
| 82 | 2 | `u16 content_type_len` in bytes |
| 84 | Variable | Exact UTF-8 key bytes followed by exact content-type header bytes |

There is no padding. `metadata_len` must equal
`84 + key_len + content_type_len`, so it is at most 1,236 bytes with the
limits below. Calculate the CRC over all `metadata_len` metadata bytes with
bytes 12 through 15 zeroed; the payload is not included. Startup first reads
the two-byte version, then the remaining fixed v1 fields. Check the version,
magic, lengths, and maximum before allocating or reading variable metadata.
Reject unknown versions without guessing their later layout. A future version
may place or encode its metadata length differently.

Limit logical keys to 1,024 UTF-8 bytes, measured after request decoding.
Enforce this in `Key::new` so every store backend has the same key contract;
exactly 1,024 bytes is valid and 1,025 bytes is rejected as an invalid
request. The filesystem header stores the exact UTF-8 bytes and rejects an
oversized key during startup validation as well. Limit the complete
content-type header value, including any parameters, to 128 bytes. Enforce
that limit in `ContentType::try_from_header` for every backend and during
filesystem startup validation. Exactly 128 bytes is valid and 129 bytes is
rejected as an invalid request. Both fields are nonempty, so the minimum
version 1 `metadata_len` is 86 bytes.

Store the payload length counted during PUT. Validate it against the opened
file's length using checked arithmetic:

```text
file_len == metadata_len + payload_len
```

The startup length check is intentional: it reads filesystem metadata but no
payload bytes. It detects a shortened or extended file during startup. Damage
that changes payload bytes without changing their count is detected only by
the explicit SHA-256 check.

The physical filename remains the lowercase SHA-256 of the validated logical
key, followed by `.obj`. The service validates the embedded key against the
derived filename before treating the file as that key's object. Client keys
are never joined to a filesystem path.

## 3. Startup scan and index

Before declaring the store ready, acquire exclusive ownership of the storage
root so startup cleanup cannot race an upload from another instance. Clear the
dedicated `part/` directory, removing leftover unpublished files without
trying to recover or publish them. Report cleanup failures through stderr and
the abnormal-event journal; a failure to clear the directory prevents
readiness. Then build the index:

1. Enumerate the configured `objects/` directory, which contains published
   files only. Reject symlinks and non-regular published entries.
2. For each candidate `.obj`, validate its filename, read the fixed preamble,
   bound `metadata_len`, then read exactly the remaining metadata bytes.
3. Validate metadata magic, version, field lengths and encodings, CRC32,
   logical key, and logical-key-to-filename match. Compare the opened file's
   length with `metadata_len + payload_len`. Do not read or hash payload bytes.
4. Retain the opened file in an `Arc<File>` for each healthy entry. Insert a
   healthy or corrupt entry into a `BTreeMap` keyed by logical key when the
   key can be recovered and verified. Log a file-specific error and skip files
   that cannot be associated with a trustworthy key. Corrupt entries do not
   retain a readable file handle.
5. Publish the completed map to request handlers. A failure to enumerate the
   directory or exhaustion of file descriptors while opening valid objects
   prevents readiness; a bad individual file is logged and does not prevent
   valid objects from being served.

Use one index with a stateful entry, conceptually:

```rust
BTreeMap<Key, IndexEntry>

enum IndexEntry {
    Healthy { metadata: ObjectMetadata, file: Arc<File> },
    Corrupt { physical_name: String, reason: CorruptReason },
}
```

Store metadata, status, and an open file handle for each healthy object in
memory, never payload bytes. Healthy entries retain enough information for
LIST and HEAD; a corrupt entry retains the recoverable key and diagnostic
reason without pretending unknown content type or size is valid. Keep the
scan's skipped-file count and errors observable in local logs. Files
that cannot yield a trustworthy key cannot appear in a map keyed by logical
key; leave them untouched and report them by physical filename.

The initial scan is O(number of published files) and reads only their bounded
metadata, but keeps one open file descriptor per healthy object. A `BTreeMap`
keeps prefix listing in logical-key order without sorting every page. Measure
cold-start time, index memory, and file-descriptor use on the target home
machine with a representative file count before adding another index.

## 4. Reads, LIST, and corrupt entries

For GET and range GET, clone the healthy entry's `Arc<File>` and copy its
metadata while holding the `BTreeMap` read lock, then release the lock before
file I/O. The handle and metadata form one snapshot of the object. Check that
handle's file length against `metadata_len + payload_len` before serving, so
`Content-Length` and `Content-Range` describe a validated length. The startup
scan has already validated the header and exact key; normal reads do not
reopen the path or parse the header again. HEAD uses the same snapshot and
length check without reading payload bytes.

Stream bounded payload chunks with Unix `std::os::unix::fs::FileExt::read_at`.
The physical offset is `metadata_len + requested_logical_offset`, calculated
with checked arithmetic. Positional reads do not change a shared file cursor,
so concurrent GETs can read through cloned `Arc<File>` handles safely. Handle
short reads by advancing the offset and retrying; treat EOF before the expected
payload end as a read error. Run blocking file reads off the async executor,
schedule only bounded chunks, and let HTTP/2 backpressure limit reads ahead.
Do not read the complete file into memory or verify its SHA-256 during normal
GET.

The existing `ReadRange` and `ReadSpan` express the selected logical offset
and size. The bounded reader interface and HTTP/2 delivery described in
[Async Object Reader Implementation Plan](async-object-reader-implementation-plan.md)
are implemented for the in-memory store. The filesystem reader will follow
that interface and its stream reset behavior for read errors after headers.

An entry marked corrupt returns the existing `StoreErrorKind::Corrupt` on GET
or HEAD. An unconditional or replace-only PUT for that key can replace the
damaged file through normal atomic publication, then change the index entry
to healthy; create-only PUT fails its precondition. A PUT that fails before
rename leaves the old file and corrupt status in place. Ordinary startup does
not delete or quarantine damaged files.

LIST continues using the existing prefix, cursor, ordering, and page-size
contract and omits corrupt entries, which cannot reliably supply its required
content type and size. A pagination cursor identifies the inclusive next key
position and is independent of the requested prefix. The public response
format is unchanged. Skipped-file diagnostics go to stderr and the local
journal.

## 5. Streaming PUT and DELETE

Keep `part/` and `objects/` as separate directories beneath the same storage
root and on the same filesystem. Name each upload file with a freshly
generated random identifier and a `.part` suffix; the name does not encode the
logical key. Create it exclusively and retry a rare name collision. A
cross-filesystem `part/` configuration is invalid because it prevents atomic
rename into `objects/`.

Use the implemented `StoreInterface::put_context(key, content_type, condition)`
and `StoreInterface::put(context)` contract. The context already owns the
validated key, content type, and `PutCondition`; the HTTP handler already
parses `If-None-Match: *` as create-only and `If-Match: *` as replace-only.
The filesystem backend must preserve those existing HTTP responses and
evaluate the condition at final publication, not when creating the context.
See [Conditional PUT Implementation Plan](conditional-put-implementation-plan.md).

Determine `metadata_len` from the bounded metadata format before writing an
upload using the owned key and content type. Reserve that many bytes in the
new `.part` file and stream payload chunks after the reserved region. Count
bytes and calculate payload SHA-256 while writing. At completion, write the
counted `u64` payload length, final metadata, and metadata CRC into the
reserved region. Sync the file, atomically rename it to the derived final
filename in `objects/`, and acknowledge success after updating the in-memory
index. Do not sync the `part/` or `objects/` directories. On cancellation or
failure before publication, try to remove the `.part` file; startup clears
any leftovers.

The lack of directory sync is an accepted durability tradeoff for this home
store. An acknowledged PUT is immediately visible, but a system crash or
power loss before the directory changes reach storage may lose the rename.
Syncing the file still protects its contents once the name is durable. The
small crash window is acceptable here. Publication does not include a
directory-sync step or a post-rename sync failure path.

After finishing the `.part` file, serialize publication with PUT and DELETE
for the same key. Under that coordination, check the current index entry and
the derived final path immediately before rename. A healthy or keyed corrupt
entry counts as present; an absent key counts as absent only when the derived
path is also unoccupied. Apply `PutCondition::Unconditional`, `CreateOnly`, or
`ReplaceOnly` against that state. On a failed condition, return
`StoreErrorKind::PreconditionFailed` without renaming or changing the index,
and remove the unpublished `.part` file. If the index is absent but an
unkeyable file occupies the derived path, preserve that file and report
`StoreErrorKind::Corrupt` for local maintenance rather than overwriting it.
The existing HTTP handler maps failed preconditions to `412`.

Coordinate the rename or delete and the corresponding `BTreeMap` update so
concurrent reads and LIST do not observe a mixed file/index state. Published
files are immutable. An in-flight GET keeps its captured `Arc<File>` and can
finish reading the old object after replacement or deletion; new GETs use the
updated entry. On restart, rebuild the map from the directory contents; there
is no index transaction or index-reconciliation phase.

DELETE remains idempotent and removes a file by its server-derived name.
Remove the index entry under the same mutation coordination and acknowledge
without syncing the directory. A system crash or power loss before the
directory change reaches storage may make a deleted file reappear after
restart; this is the same accepted durability tradeoff. A keyed corrupt entry
follows the same DELETE behavior as a healthy entry: remove its published file
and index entry, then report success. Record the abnormal deletion in the local
event journal.
If a file exists at the key-derived path without an index entry, DELETE also
removes it and records that abnormal deletion in the journal.

## 6. Abnormal-event journal

Keep a local journal of abnormal storage events so they remain visible after
the affected object is repaired or deleted. At minimum, record startup
corruption findings, including unkeyable files identified by physical
filename and reason; GET or HEAD attempts against a corrupt entry; deletion
of a corrupt entry or an unindexed file at a key-derived path.

Write each event as one UTF-8 JSON Lines object with a UTC RFC 3339 `time`, a
stable `event` name, the logical `key` or physical `file` when available, and
a `reason`. Include the object UUID as `object_id` when it was validated, so
events can distinguish replacements at the same key. Additional event-specific
fields may be included. Do not include payload bytes. Emit the same JSON line
to stderr and append it to the local journal file on a best-effort basis. For
example:

```json
{"time":"2026-09-26T08:30:00Z","event":"corrupt_file_skipped","file":"abc.obj","reason":"metadata CRC mismatch"}
```

If the journal file cannot be opened or written, report that failure to stderr
and continue the startup scan or object operation with its normal result.
Journal availability does not affect readiness or make a successful DELETE
fail. The journal does not change the public object interface or serve as an
index. Keep the journal path configurable. Use the host's `logrotate` with
configurable `size` and `rotate` settings, defaulting to `size 10M` and
`rotate 5`; keep numbered files such as `journal.1` and `journal.2`. Open the
current journal path for each append so writes move to the new file after
rotation; do not use `copytruncate`. Rotation happens when `logrotate` runs,
so `size 10M` is a trigger rather than a strict file-size cap.

## 7. Verification

- Build a temporary storage root containing valid objects, stale `part/` files,
  malformed metadata, a bad metadata CRC, an invalid filename, and a file
  shortened or extended relative to its stored payload length. Confirm startup
  clears `part/`, indexes valid objects, marks recoverable keyed failures
  corrupt, logs and skips unkeyable files, and reports directory enumeration
  or part cleanup failures as unavailable.
- Exercise prefix pagination over thousands of synthetic metadata headers and
  measure cold-start time, resident index memory, and open file-descriptor use
  on the target machine. Confirm descriptor exhaustion prevents readiness
  rather than misclassifying healthy files as corrupt.
- Upload, replace, range-read, and delete objects while LIST and GET run
  concurrently. Check that responses match a complete old or new object, not
  mixed metadata and bytes. Run simultaneous range reads against one object,
  including a read spanning replacement, and confirm each keeps a consistent
  snapshot. Confirm short reads and early EOF are reported as read errors.
- Change payload bytes without changing file length and confirm startup's
  metadata scan does not hash them.
- Exercise unconditional, create-only, and replace-only PUT against absent,
  healthy, and keyed corrupt entries. Confirm a failed condition returns
  `PreconditionFailed`, leaves the old file and index entry unchanged, and
  removes the unpublished `.part` file. A successful repair becomes healthy
  after rename and the matching index update; an interruption before rename
  preserves the old state.
- Race two create-only PUTs for one absent key and confirm exactly one
  publishes. Race conditional PUT with DELETE and confirm the condition uses
  the serialized state at final publication. Preserve an unkeyable file that
  occupies the derived final path and report corruption rather than replacing
  it.
- Confirm public LIST omits keyed corrupt entries without changing pagination.
- DELETE a keyed corrupt entry and confirm it removes the file and index entry
  with the same response as a healthy deletion, while recording an abnormal
  event to stderr and attempting to append it to the local journal. Make the
  journal unwritable and confirm the operation still succeeds and stderr
  reports both the event and the journal failure.
- Confirm an unkeyable file produces a stderr and journal event containing its
  physical filename and reason. Rotate the journal and confirm later events
  append to the new current file. Confirm the lines parse as JSON and include
  the required fields.
- Confirm uploads use random, exclusively created names in `part/`; a failed
  or cancelled upload never appears in `objects/`; a completed upload survives
  restart; and a cross-filesystem `part/` configuration is rejected.
- Confirm the filesystem PUT context uses the existing owned key, content
  type, and condition without changing the store or HTTP interface.
- Accept a 1,024-byte UTF-8 key and reject a 1,025-byte key through `Key::new`
  and the HTTP route; count bytes rather than Unicode characters.
- Accept a 128-byte content type and reject a 129-byte value through
  `ContentType::try_from_header` and the HTTP route, including parameters in
  the byte count.
- Round-trip the exact version 1 header, including `OBJSTORE`, CRC zeroing,
  the 86-byte minimum and 1,236-byte maximum, and the version-first parser.
  Reject unknown versions without interpreting their later bytes.

Run the focused `journey-storage` checks and tests. Do not run `cargo fmt`,
`rustfmt`, or another automated source formatter in this repository.
