# Home Object Storage Interface

**Status:** Preliminary interface agreement  
**Created:** 24 September 2026  
**Scope:** Semantic client, recovery, and local administration interfaces for
the first Journey home object store

## 1. Purpose

This document defines what the Journey home object store does without yet
fixing its HTTP routes, HTTP header mapping, Rust traits, or SQLite schema.

The interface sits above the physical container and index described in
[Home Object Storage Design](home-object-storage-design.md). A caller works
with logical object keys, metadata, and byte streams. It does not see physical
filenames, `.part` files, embedded headers, or database rows.

The first interface has three distinct surfaces:

1. The private object interface carried from AWS to home over the existing
   inner HTTP/2 connection.
2. Local recovery and maintenance commands on the home server.
3. A separate read-only HTML administration page reachable only from the
   trusted home network.

These surfaces must not accidentally grant one another's authority.

## 2. Object model

An object has immutable storage metadata:

```text
logical_key
object_id
content_type
payload_length
payload_sha256
created_at
```

The client supplies:

- the logical key;
- the content type;
- the payload byte stream;
- optionally, the expected payload length; and
- optionally, the expected payload SHA-256.

The home service generates the opaque object ID, calculates the actual length
and SHA-256, and assigns the creation time.

Application metadata remains outside the home object interface:

| Storage metadata | AWS application metadata |
| --- | --- |
| Logical object key | Owning post |
| Content type | Caption and alt text |
| Payload length | Display order |
| Payload SHA-256 | Publication state |
| Object ID | Page and user relationships |
| Creation time | Presentation filename or title |

The home service does not interpret pictures, videos, posts, captions, or
publication state.

## 3. Operation set

The preliminary private object interface contains:

```text
CREATE
READ
STAT
LIST
COPY
DELETE
HEALTH
```

`CREATE`, `READ`, `STAT`, `LIST`, `COPY`, and `DELETE` operate on logical keys.
`HEALTH` describes service readiness and capability, not individual objects.

## 4. Create

Conceptually:

```text
create(
    logical_key,
    content_type,
    expected_length?,
    expected_sha256?,
    payload_stream
) -> object_metadata
```

### 4.1 Successful creation

A successful result means:

- the complete payload has been consumed;
- actual length and SHA-256 have been calculated;
- declared length and SHA-256, when supplied, match;
- the embedded header and payload have been durably published;
- the live index contains the object; and
- a subsequent operation on the same ready home service can observe it.

The client must not infer success merely because it finished transmitting the
request body. The home response is the durability acknowledgement.

### 4.2 Existing-key behavior

Creation does not implicitly replace an existing object.

| Existing state | Result |
| --- | --- |
| Key absent | Create and return the new metadata |
| Key present with identical immutable metadata and payload digest | Idempotent success returning the existing metadata |
| Key present with different payload or immutable metadata | Conflict |

This makes whole-operation retries safe while preventing accidental overwrite.
An implementation may reject an obvious conflict before reading the complete
body. If it must receive the body to establish identity, it still must not
publish a second conflicting object.

### 4.3 Streaming and cancellation

The home service writes the body incrementally and never collects a complete
object in memory. It applies backpressure through HTTP/2 receive flow control.

On cancellation, connection loss, timeout, length failure, digest failure, or
storage failure:

- no final object becomes visible;
- no index row is committed for the failed object; and
- the temporary upload is removed immediately or left for conservative stale
  upload cleanup.

## 5. Read

Conceptually:

```text
read(logical_key, range?) -> object_metadata + payload_stream
```

The returned stream contains only original payload bytes. Physical container
headers are never returned through the object interface.

The initial range model supports one of:

```text
complete payload
bytes=start-end
bytes=start-
bytes=-suffix_length
```

Multiple ranges in one request are not initially supported. A valid range
returns the selected payload bytes and describes positions relative to the
payload, not the physical container file.

The service validates the stored header, requested logical key, physical
length, and requested range before or while establishing the stream. It does
not calculate the complete payload digest on every ordinary read.

A cancelled or slow reader stops or backpressures the file-reading task. The
service must not continue reading and buffering an unwanted object.

## 6. Stat

Conceptually:

```text
stat(logical_key) -> object_metadata
```

`STAT` returns immutable storage metadata without returning payload bytes. It
supports retry decisions, HTTP `HEAD`, reconciliation, and administration.

The operation validates at least:

- the server-derived physical filename;
- the embedded logical key;
- the supported header version;
- the header checksum; and
- consistency between declared payload length and physical file length.

Full payload hashing belongs to an explicit integrity scan rather than normal
`STAT` behavior.

## 7. List

Conceptually:

```text
list(prefix, cursor?, limit) -> entries + next_cursor?
```

`LIST` operates on the logical key namespace and is served from the SQLite
index. `/` has no special storage meaning; it is an ordinary key character
that lets clients present prefix hierarchies.

Initial semantics are:

- prefix matching from the beginning of the logical key;
- binary lexicographic ordering of UTF-8 key bytes;
- a bounded client-requested page size;
- a service-enforced maximum page size;
- an opaque cursor that callers must not interpret; and
- metadata entries without payload data.

There is no recursive flag or synthetic directory object initially. A client
can infer a folder-like view by examining key prefixes and delimiters.

A cursor is valid only for the listing contract and database generation rules
defined by the eventual wire design. The first implementation need not promise
a transactionally frozen snapshot across pages while concurrent mutations
occur. It must not repeat or skip entries in an otherwise unchanged index.

## 8. Copy and logical rename

Conceptually:

```text
copy(source_key, destination_key) -> destination_metadata
```

`COPY` performs a server-local payload copy. It avoids sending a large picture
or video from home through AWS and back to home.

Initial semantics are:

- the source must exist and pass normal header validation;
- the destination must not contain a conflicting object;
- an identical existing destination is an idempotent success;
- the destination receives a new object ID, creation time, logical key, and
  embedded header;
- payload length, content type, payload bytes, and payload SHA-256 are
  preserved; and
- failure leaves the source untouched and publishes no partial destination.

Journey does not initially define an atomic rename operation. The client
implements an S3-like rename as:

```text
COPY old_key new_key
DELETE old_key
```

Both keys may exist between those operations. If deletion fails, retrying it
is safe.

## 9. Delete

Conceptually:

```text
delete(logical_key) -> { removed: boolean }
```

Deletion is client-directed and idempotent:

| Existing state | Result |
| --- | --- |
| Key present and durably removed | Success with `removed = true` |
| Key absent | Success with `removed = false` |

The client supplies only a logical key. The home service derives and validates
the physical object; it never removes a client-supplied filesystem path.

A successful response means the selected durable deletion point has been
reached for both the published object and index. The exact internal ordering
and any temporary trash mechanism are implementation details, but restart
reconciliation must resolve an interruption between filesystem and index
changes.

The first interface has no automatic retention, reference counting, garbage
collection, or lifecycle policy. AWS decides when to request deletion.

## 10. Health

Conceptually:

```text
health() -> service_health
```

Useful health information includes:

```text
ready
container_format_version
index_schema_version
index_ready
storage_readable
storage_writable
available_bytes
```

`HEALTH` must be cheap. It does not scan all containers or hash payloads. The
service does not report ready while mandatory startup reconciliation or schema
migration prevents safe object operations.

## 11. Error categories

The semantic interface distinguishes at least:

| Category | Meaning |
| --- | --- |
| Invalid request | Malformed key, metadata, cursor, range, length, or digest |
| Not found | Requested source object does not exist |
| Conflict | A different immutable object already owns the key |
| Range unsatisfiable | A valid range cannot select bytes from this payload |
| Corrupt object | Header, filename, file length, or verified payload integrity failed |
| Capacity rejected | Object size, quota, disk space, concurrency, or another configured limit was exceeded |
| Cancelled | The caller or connection ended the operation |
| Temporarily unavailable | Storage or index cannot safely serve the operation now |
| Internal storage failure | An unexpected filesystem, SQLite, or durability operation failed |

The wire protocol will map these categories to bounded HTTP statuses and
machine-readable response bodies later. Internal paths and sensitive operating
system error details must not be returned to remote clients.

## 12. Concurrency and consistency

Operations that can change the same logical key are serialized:

```text
CREATE(key)
COPY(_, key)
DELETE(key)
```

`COPY(source, destination)` participates in coordination for both keys so that
the source cannot disappear midway through a successful copy and competing
destination writes cannot both publish.

Published containers are immutable, so concurrent reads of an existing object
do not require exclusive access. Global and per-operation limits bound:

- active uploads;
- active downloads;
- active copies;
- open files;
- HTTP/2 streams;
- in-memory chunks;
- maximum object size; and
- listing page size.

After a successful mutation response, later operations through the same ready
service observe the mutation. The first single-server implementation does not
need distributed consistency semantics.

The filesystem container is the durable recovery record. SQLite is the live
query index. The service must reconcile an object published just before a crash
but not yet indexed, and an interrupted deletion whose filesystem and database
steps did not both complete.

## 13. SQLite live index

SQLite is the operational index for:

- exact key lookup;
- prefix listing and pagination;
- uniqueness constraints;
- administration queries; and
- fast reconciliation decisions.

It is not the only durable copy of object identity and immutable metadata.
Published object headers contain the fields required to reconstruct the index.

The initial implementation should use prepared statements, bounded queries,
transactions, and a deliberately small connection model such as one dedicated
database task or a small explicitly bounded pool. SQLite access must not block
the asynchronous runtime's worker threads for unbounded periods.

The final schema, journaling mode, busy policy, transaction ordering, and
database task design are deferred implementation decisions.

## 14. JSON manifest

Journey provides a human-readable JSON snapshot of the SQLite index. The
manifest is useful for inspection, backup checks, migrations, and recovery,
but it is not the live index and may lag the newest acknowledged mutations
unless explicitly regenerated.

A preliminary shape is:

```json
{
  "format": "journey-object-manifest",
  "version": 1,
  "generated_at": "2026-09-24T10:30:00Z",
  "object_count": 1,
  "objects": [
    {
      "key": "images/2026/holiday/beach.jpg",
      "physical_hash": "80e55ea1...",
      "object_id": "019d...",
      "content_type": "image/jpeg",
      "payload_length": 2840137,
      "payload_sha256": "d80f...",
      "created_at_ms": 1790225930000
    }
  ]
}
```

Manifest requirements are:

- a versioned top-level format;
- objects sorted by logical key for deterministic output;
- no credentials, absolute paths, or internal temporary names;
- generation through a consistent SQLite read transaction;
- writing to a temporary file before atomic replacement;
- synchronization appropriate for its use as a recovery artefact; and
- storage outside the published `objects/` directory.

The recovery authority order is:

```text
embedded headers -> authoritative immutable object records
SQLite           -> live operational index
JSON manifest    -> human-readable point-in-time snapshot
```

A single JSON document is sufficient for the anticipated scale. JSON Lines may
be added later for very large or streaming exports without replacing SQLite.

## 15. Local maintenance interface

Recovery and expensive maintenance operations are local administrative
commands, not remote object operations. The intended command set includes:

```text
journey-storage export-manifest
journey-storage verify
journey-storage rebuild-index
journey-storage inspect <object>
journey-storage extract <object> <destination>
journey-storage list-stale-parts
```

These commands operate under explicit local administrator authority. They must
report damage before making destructive changes. Index rebuild should create a
new database and install it only after successful validation rather than
modifying a questionable index in place.

## 16. Read-only home-LAN administration page

The home service provides a small server-rendered HTML administration page on
a separate listener reachable only from the trusted home network.

Its initial purpose is observation:

- search objects by exact key or prefix;
- paginate through indexed objects;
- view immutable metadata;
- view aggregate object counts and payload sizes;
- view filesystem capacity and index health;
- identify index/container mismatches already discovered by maintenance;
- report stale temporary files; and
- show the last JSON manifest generation time and result.

Possible internal pages are:

```text
/admin/objects?prefix=videos/2026/&limit=100
/admin/objects/<object-id>
/admin/storage
/admin/health
/admin/manifests
```

These are illustrative page routes, not the final URL contract.

### 16.1 Initial restrictions

The first page is strictly read-only. It provides:

- no raw SQL input;
- no object delete, copy, or rename controls;
- no metadata mutation;
- no index rebuild button;
- no arbitrary filesystem browser;
- no unbounded result set; and
- no link that exposes the public AWS or private WebSocket credentials.

All queries use prepared statements, explicit ordering, bounded limits, and
HTML escaping. Logical keys and metadata are untrusted display text.

An unrestricted SQL textbox is intentionally excluded. Raw diagnostic SQL, if
needed during development, is performed locally with standard SQLite tooling
and direct host access.

### 16.2 Network boundary

The administration listener is not carried through the AWS-to-home WebSocket,
not routed by public AWS Nginx, and not exposed on the public Internet.

The intended deployment is:

```text
trusted home-LAN browser
    -> home LAN address and dedicated admin port
    -> read-only Journey admin handler
    -> bounded SQLite queries
```

The container publishes the port only on the intended home LAN interface where
practical, and the host firewall restricts it to the trusted subnet. Binding to
`0.0.0.0` without an equivalent firewall rule is not the desired deployment.

A home LAN is not an authentication boundary by itself. Basic authentication
should be added before the page contains real personal filenames, media links,
or other private information. Read-only behavior reduces impact but does not
remove the confidentiality concern posed by guest or compromised local devices.

The administration handler accesses SQLite through normal application database
connections. It never copies or parses the live database file behind SQLite's
locking and transaction mechanisms.

## 17. Interface boundaries

The private object interface may mutate storage but exposes only the approved
object operations. It does not expose maintenance commands or the admin page.

The local maintenance interface may perform expensive verification and index
reconstruction but is not remotely routable.

The LAN administration page initially observes SQLite and health state but may
not mutate either objects or the index.

```text
AWS gateway
    -> WSS and inner HTTP/2
    -> CREATE / READ / STAT / LIST / COPY / DELETE / HEALTH

home shell
    -> local maintenance commands

trusted home LAN
    -> separate read-only HTML administration listener
```

This separation is part of the security design rather than only a deployment
convenience.

## 18. Deferred decisions

The following remain deliberately undecided until implementation planning:

- exact HTTP methods, routes, request headers, and response bodies;
- exact Rust traits and ownership types;
- key transport encoding in HTTP paths or fields;
- SQLite schema and journaling mode;
- page cursor encoding and mutation behavior across paginated requests;
- exact object-size, concurrency, timeout, and page-size limits;
- detailed create conflict optimization when the request body has not arrived;
- durable deletion implementation and restart reconciliation;
- authentication configuration for the LAN administration listener; and
- manifest scheduling and retention.

These decisions may refine the wire and implementation without changing the
semantic operation set agreed in this document.
