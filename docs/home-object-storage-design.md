# Home Object Storage Design

**Status:** Preliminary design for discussion and implementation planning  
**Created:** 24 September 2026  
**Scope:** The first filesystem-backed Journey object store on the home server

The semantic client and administration surfaces are defined separately in
[Home Object Storage Interface](home-object-storage-interface.md).

## 1. Purpose

Journey needs a deliberately small object store for pictures and videos. It
does not need a general S3 server, a storage abstraction layer, replication,
or an independently administered database service.

The design must provide:

- streaming uploads and downloads with bounded memory;
- HTTP byte-range reads for video;
- user-visible keys with S3-like `/`-separated prefixes;
- strict confinement to one configured storage directory;
- atomic publication of completed uploads;
- enough metadata in every stored object to rebuild the index after loss of
  the index database; and
- ordinary filesystem operations that remain understandable and recoverable.

The design uses `tokio::fs`, the existing inner HTTP/2 connection, a flat set
of Journey object-container files, and a rebuildable SQLite index.

## 2. Deliberate non-goals

The first implementation does not attempt to provide:

- complete Amazon S3 API or object-key compatibility;
- buckets, accounts, IAM, object versions, multipart uploads, or lifecycle
  rules;
- replication, erasure coding, deduplication, or automatic backup;
- arbitrary filesystem paths supplied by a client;
- mutable metadata inside published object files; or
- an abstraction over multiple storage backends.

These omissions are intentional. The home service remains a narrow Journey
component rather than a general-purpose storage product.

## 3. Logical keys and physical names

### 3.1 Logical key

The client assigns each object a logical key such as:

```text
images/2026/holiday/beach.jpg
videos/2026/holiday/beach.mp4
```

The `/` characters provide a logical prefix structure for listing and display.
They do not become operating-system path separators on the home server.

Journey should define a smaller and less ambiguous key language than full S3.
The initial validator should enforce at least:

- a non-empty UTF-8 key;
- a maximum encoded length of 300 bytes;
- no NUL or control characters;
- no backslash;
- no leading or trailing `/`;
- no empty `/`-separated component; and
- no component equal to `.` or `..`.

These restrictions primarily keep routing, logging, recovery tooling, and
cross-platform clients predictable. Filesystem confinement does not depend on
them because the logical key is never joined to the storage root.

The protocol must decode the key exactly once before validation. It must not
silently normalize one key into another.

### 3.2 Physical filename

The physical filename is the lowercase hexadecimal SHA-256 digest of the
validated logical key's UTF-8 bytes:

```text
physical_name = hex(sha256(logical_key_utf8)) + ".jobj"
```

For example:

```text
logical key:   images/2026/holiday/beach.jpg
physical file: objects/80e55ea1d0b7...f27.jobj
```

Only the server computes the physical filename. A client never supplies a
filesystem path. Every opened published object therefore has a name matching:

```text
[0-9a-f]{64}.jobj
```

The complete storage directory initially remains flat:

```text
storage/
└── objects/
    ├── 0d41...a17.jobj
    ├── 80e5...f27.jobj
    └── .journey-upload-<random>.part
```

If one directory eventually contains too many entries, the service may derive
shard directories from the first digest bytes, for example `80/e5/<digest>`.
Those directories remain server-derived and do not contain client path text.

### 3.3 Collision defence

A SHA-256 collision is not expected in practice, but the implementation must
not rely on probability for identity correctness. After opening a physical
file, Journey compares the exact logical key in its embedded header with the
requested key. A mismatch is a storage-integrity error, not the requested
object.

## 4. Journey object-container file

A published file contains a fixed 512-byte Journey header followed immediately
by the original media bytes:

```text
byte 0
┌──────────────────────────────┐
│ 512-byte Journey header      │
├──────────────────────────────┤
│ original picture/video bytes │
└──────────────────────────────┘
```

The file is a Journey container, not a directly playable JPEG, WebP, or MP4.
Journey and its recovery tool expose or extract only the payload.

### 4.1 Preliminary header layout

All integers use one explicitly documented byte order. Little-endian is the
initial choice. Unused bytes must be zero when written and ignored when read,
unless a later format version assigns them meaning.

| Offset | Size | Field |
| ---: | ---: | --- |
| 0 | 8 | Magic bytes `JRNYOBJ\0` |
| 8 | 2 | Container format version |
| 10 | 2 | Flags; initially zero |
| 12 | 4 | CRC32 of the complete header with this field zeroed |
| 16 | 8 | Payload length in bytes |
| 24 | 8 | Creation time as Unix milliseconds |
| 32 | 32 | SHA-256 of the payload bytes |
| 64 | 16 | Opaque object UUID in binary form |
| 80 | 2 | Logical-key length in bytes |
| 82 | 2 | Content-type length in bytes |
| 84 | 428 | Logical key, content type, then zeroed reserved bytes |

The variable region contains exactly:

```text
logical_key_utf8 || content_type_ascii || zero_padding
```

Initial limits are:

- logical key: at most 300 bytes;
- content type: at most 100 bytes; and
- combined variable data: at most 400 bytes.

The remaining 28 bytes are reserved. The parser must enforce the individual
limits, the combined limit, valid encodings, the supported version, the header
CRC, and consistency between the payload length and physical file length.

The header CRC detects accidental header damage. The payload SHA-256 detects
payload damage and supplies an integrity value; it does not make the container
an authenticated or encrypted format.

### 4.2 Immutability

The 512-byte header is written once before publication and is not modified in
place afterward. A 512-byte aligned filesystem write is not assumed to be
atomic across a process crash, kernel crash, or power failure.

Any operation that needs different embedded metadata creates another complete
container through the normal temporary-file and publication process. This
avoids turning a torn in-place header write into an unrecoverable published
object.

## 5. Streaming upload

The home service writes an upload without holding the complete object in
memory:

1. Validate the logical key and request metadata.
2. Compute the final physical filename from the logical key.
3. Create a server-named, unique `.part` file with exclusive creation inside
   the object-storage filesystem.
4. Leave or write a 512-byte placeholder region.
5. Stream HTTP/2 DATA chunks into the file beginning at physical offset 512.
6. Enforce the maximum payload length while receiving.
7. Update the payload length and SHA-256 incrementally.
8. Release HTTP/2 receive capacity only after each chunk has been successfully
   consumed by the file-writing path.
9. If a declared content length exists, compare it with the received length.
10. Seek to byte zero and write the complete 512-byte header.
11. Synchronize the file contents and metadata.
12. Atomically rename the `.part` file to the final physical filename.
13. Synchronize the containing directory so the rename is durable.
14. Report success only after the required durability steps complete.

An upload error, cancellation, length mismatch, checksum failure, or transport
loss must never publish the `.part` file. Startup maintenance may remove stale
`.part` files according to a conservative age rule.

Temporary and final files must reside on the same filesystem. A cross-filesystem
move is not an atomic publication mechanism.

## 6. Crash atomicity and durability

Atomicity and durability are separate properties:

- **Atomic publication** means readers see either no final object or one
  complete final object. They never see a partially written final object.
- **Durability** means an acknowledged object and its final directory entry
  survive a power loss and reboot.

Journey obtains both with this ordering:

```text
write payload and final header to .part
    -> sync the .part file
    -> rename .part to the final name on the same filesystem
    -> sync the containing directory
    -> acknowledge success
```

Expected outcomes at interruption points are:

| Interruption point | Expected visible state |
| --- | --- |
| During payload or header writing | An incomplete unpublished `.part` file |
| After file sync, before rename | A complete unpublished `.part` file |
| After rename, before directory sync | The rename may not be durable across power loss; do not acknowledge yet |
| After directory sync and acknowledgement | The complete final object is durable under the filesystem's guarantees |

The header CRC helps detect a damaged header but cannot restore an old header.
It is not a substitute for temporary-file publication.

The exact durability guarantee ultimately depends on the host filesystem,
mount configuration, storage device, and whether the device correctly honors
flush requests. The first deployment should use a normal local filesystem with
well-understood rename and synchronization behavior.

## 7. Streaming reads and video ranges

The service reads and validates the 512-byte header before returning an object.
Only payload bytes are exposed to clients.

A complete download reads from physical offset 512 for exactly
`payload_length` bytes. A logical payload range translates to:

```text
physical_start = 512 + requested_payload_start
physical_length = requested_payload_end - requested_payload_start + 1
```

HTTP response lengths and `Content-Range` values always describe the payload,
not the complete container file. The service must never stream the header or
any bytes beyond the declared payload length.

The existing HTTP/2 flow-control approach remains appropriate:

- reserve send capacity;
- wait for capacity;
- read no more than the available capacity and remaining range;
- send that bounded chunk; and
- repeat until the payload or requested range is complete.

Memory use is consequently bounded by configured per-stream and transport
buffers rather than object size.

## 8. SQLite as a rebuildable index

SQLite provides efficient lookup and prefix listing, but it is not the sole
copy of storage metadata. Each published container contains enough immutable
metadata to recreate its index row.

The initial index can include:

- exact logical key;
- physical key digest or filename;
- object UUID;
- payload length;
- content type;
- payload SHA-256;
- creation time; and
- index or scan timestamps that do not affect object identity.

The database must not contain the only copy of any field required to identify,
validate, or serve a stored object.

### 8.1 Rebuild procedure

A rebuild performs:

1. Create a new SQLite database rather than modifying the damaged one.
2. Enumerate regular published object files only.
3. Reject unexpected filenames and ignore `.part` files.
4. Read exactly the fixed header.
5. Validate magic, version, lengths, encodings, CRC, physical file length,
   physical filename, and logical-key hash.
6. Insert valid metadata into the new index inside bounded transactions.
7. Report duplicate keys, duplicate UUIDs, collisions, and invalid containers
   instead of silently selecting one.
8. Quarantine or leave invalid files untouched according to an explicit
   recovery policy.
9. Atomically install the rebuilt database after the scan succeeds.

Full payload hashing during every index rebuild may be optional because it can
be expensive for large videos. A separate integrity scan can verify payload
SHA-256 values incrementally. Header and file-length validation remain required
during an ordinary rebuild.

The operational JSON manifest and read-only home-LAN administration page that
query this index are specified in
[Home Object Storage Interface](home-object-storage-interface.md).

## 9. Rename and delete semantics

The client owns the decision to rename or delete an object. The home service
performs only the requested bounded storage operation after validating the
logical key.

S3 does not provide an atomic object rename; the equivalent is copy followed by
delete. Journey initially follows the same simple model:

1. Create and durably publish a container with the new logical key and header.
2. Add the new index entry.
3. Remove the old index entry and old container when instructed.

This may copy the payload locally for a large object, but it preserves the
immutable-header and database-rebuild guarantees. Renames are expected to be
rare compared with reads. A future indirection or alias design may optimize
renames only if measurement shows that it matters.

Deletion computes the server-derived physical filename, opens and validates
the embedded logical key, removes the published file, and synchronizes the
containing directory when durable deletion is required. The service must not
delete an arbitrary client-supplied pathname.

## 10. Path-confinement security boundary

The primary path-confinement property is structural:

```text
untrusted logical key
    -> validation
    -> SHA-256
    -> server-constructed hexadecimal filename
    -> configured storage root
```

The logical key is never passed to `Path::join`, `File::open`, `rename`, or
`remove_file`. Sequences such as `/`, `..`, URL escapes, Unicode, and shell
characters cannot select a filesystem location because they appear only in
the hash input and embedded header.

Additional protections remain necessary:

- canonicalize and validate the configured storage root at startup;
- run as an unprivileged service user;
- mount only the required storage directory into the container;
- keep the storage root private from unrelated writers;
- use exclusive creation for temporary files;
- reject symlinks and non-regular files when opening published objects;
- do not follow client-provided redirects or alternate paths; and
- validate both logical keys involved in copy/rename behavior.

Because only server-derived names are used, Journey does not need to reproduce
the complex path sanitization of a filesystem-mapped S3 server.

## 11. Failure handling and recovery tooling

The implementation must distinguish at least:

- logical key not found;
- invalid or ambiguous key;
- existing-key conflict according to the selected write policy;
- malformed or unsupported container header;
- header or payload integrity failure;
- insufficient storage space or quota;
- I/O and synchronization failure;
- interrupted HTTP/2 stream; and
- uncertain completion after a late publication error.

Recovery tooling should be able to:

- inspect and print a container header;
- extract the original payload;
- verify header and payload checksums;
- identify stale temporary files;
- rebuild the SQLite index; and
- report invalid objects without automatically deleting them.

The recovery format is versioned from its first implementation. Parser limits
must be enforced before allocating based on header values.

## 12. Preliminary decisions

The current direction is:

- custom Journey storage using `tokio::fs`;
- no OpenDAL, RustFS, Garage, MinIO, or other storage daemon initially;
- S3-like logical prefixes without mapping them to filesystem directories;
- flat, server-derived physical filenames based on the logical-key hash;
- one fixed 512-byte recoverable binary header per object;
- immutable published containers;
- `.part`, file sync, same-filesystem rename, and directory sync publication;
- SQLite as a derived, rebuildable index; and
- client-directed copy/delete behavior for logical renames.

Before implementation, the exact binary header fields, key grammar, existing-key
policy, SQLite schema, late-error responses, and recovery command interface
still need focused review and tests.
