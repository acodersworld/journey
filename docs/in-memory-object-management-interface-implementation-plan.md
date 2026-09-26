# In-Memory Object Management Interface Implementation Plan

**Status:** Ready for implementation  
**Created:** 25 September 2026  
**Scope:** Backend interface and in-memory implementation for object metadata,
listing, and deletion

## 1. Goal

Expand `journey-storage` from GET/PUT into the next backend-only interface:

```text
PUT
GET
STAT
LIST
DELETE
```

This increment adds shared key/type/size metadata, typed storage errors,
bounded pagination from an inclusive global key position, and the
corresponding in-memory behavior. It does not add new HTTP routes; existing GET
and PUT are adapted only as required by the revised backend interface.

COPY and logical rename are removed from Journey's planned operation set. The
project does not expect to need optimized server-local copy in the foreseeable
future.

## 2. Scope boundaries

Implement:

- `ObjectMetadata` containing key, content type, and payload length;
- GET returning metadata and a backend-specific payload object;
- metadata-only STAT;
- prefix-filtered, bounded LIST with an opaque inclusive start-key cursor;
- idempotent DELETE;
- typed storage errors;
- configurable in-memory maximum list-page size;
- in-memory implementations of these operations;
- compatibility changes to existing HTTP/2 GET and PUT; and
- documentation cleanup for COPY and obsolete create-only behavior.

Defer:

- HTTP routes for STAT, LIST, or DELETE;
- position/size and byte-range reads;
- conditional PUT or DELETE;
- COPY and logical rename;
- health and capacity reporting;
- hashes, ETags, IDs, and timestamps;
- filesystem storage, SQLite, manifests, and persistence; and
- authentication or WebSocket wiring.

GET continues returning complete in-memory `Bytes`. Position/size reads and a
streaming filesystem reader are later interface revisions.

## 3. Metadata and read results

### 3.1 ObjectMetadata

Add:

```rust
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectMetadata {
    key: Key,
    content_type: ContentType,
    payload_length: u64,
}
```

Expose borrowed accessors for the key and content type and a copied `u64`
payload length. Keep fields private. Provide a constructor usable by future
external backend implementations without permitting inconsistent field values.

Payload length always describes user payload bytes, never a future Journey
container header.

Update supporting values:

- derive `PartialOrd` and `Ord` for `Key`;
- add `Key::as_str()` for prefix/cursor comparisons without allocation; and
- derive `PartialEq` and `Eq` for `ContentType`.

### 3.2 ReadObject

Add:

```rust
pub struct ReadObject<O> {
    metadata: ObjectMetadata,
    object: O,
}
```

Expose `metadata()`, `object()`, and `into_parts()`.

GET returns metadata with its payload because response consumers need content
type and length while reading. STAT provides metadata without payload. This is
the same useful distinction as S3 `GetObject` versus `HeadObject`.

Reduce `ObjectInterface` to the backend-specific payload operation currently
required:

```rust
pub trait ObjectInterface: Send + Sync + 'static {
    fn contents(&self) -> &Bytes;
}
```

Content type moves from `ObjectInterface` into `ReadObject::metadata`.

## 4. Typed storage errors

Add stable categories:

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StoreErrorKind {
    InvalidRequest,
    NotFound,
    Conflict,
    Capacity,
    Corrupt,
    Unavailable,
    Internal,
}
```

`Conflict` is reserved for later conditional operations. Do not add
range-unsatisfiable or cancelled until those storage contracts exist.

Add `StoreError` containing a public kind and private diagnostic detail:

- public constructors usable by future backend crates;
- `kind() -> StoreErrorKind`;
- `Debug`, `Display`, and `std::error::Error`;
- `Display` suitable for local diagnostic logging; and
- no API encouraging remote disclosure of the detail.

HTTP code selects behavior from `kind()` and never returns `Display` text to a
peer. Change storage methods and `PutContextInterface::append` from `String` to
`StoreError`. Key and content-type validation retain their dedicated errors.

## 5. StoreInterface

The interface becomes conceptually:

```rust
pub trait StoreInterface: Send + Sync + Sized + 'static {
    type Object: ObjectInterface;
    type PutContext: PutContextInterface + Send + Sync + 'static;

    fn get(&self, key: &Key)
        -> impl Future<Output = Result<ReadObject<Self::Object>, StoreError>> + Send;

    fn stat(&self, key: &Key)
        -> impl Future<Output = Result<ObjectMetadata, StoreError>> + Send;

    fn list(&self, request: ListRequest)
        -> impl Future<Output = Result<ListPage, StoreError>> + Send;

    fn delete(&self, key: &Key)
        -> impl Future<Output = Result<(), StoreError>> + Send;

    fn put_context(&self, content_type: ContentType)
        -> impl Future<Output = Result<Self::PutContext, StoreError>> + Send;

    fn put(&self, key: &Key, context: Self::PutContext)
        -> impl Future<Output = Result<(), StoreError>> + Send;
}
```

Remove `len` and `is_empty` from the common trait. They are not portable object
operations and do not constitute a useful health contract.

Use S3-like semantics:

| Operation | Key state | Result |
| --- | --- | --- |
| GET | Present | Metadata and payload object |
| GET | Absent | `NotFound` |
| STAT | Present | Metadata only |
| STAT | Absent | `NotFound` |
| PUT | Absent | Create and succeed |
| PUT | Present | Atomically replace and succeed |
| DELETE | Present | Remove and succeed |
| DELETE | Absent | Succeed identically |

PUT and DELETE return unit. Call STAT when authoritative metadata is required.
DELETE deliberately does not reveal whether an object existed.

References:

- [S3 GetObject](https://docs.aws.amazon.com/AmazonS3/latest/API/API_GetObject.html)
- [S3 HeadObject](https://docs.aws.amazon.com/AmazonS3/latest/API/API_HeadObject.html)
- [S3 DeleteObject](https://docs.aws.amazon.com/AmazonS3/latest/API/API_DeleteObject.html)

## 6. Listing contract

Add:

```rust
pub struct ListRequest {
    prefix: String,
    cursor: Option<ListCursor>,
    requested_limit: NonZeroUsize,
}

pub struct ListPage {
    objects: Vec<ObjectMetadata>,
    next_cursor: Option<ListCursor>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListCursor {
    prefix: String,
    last_key: Key,
}
```

Cursor fields are private. Callers may clone and return a cursor but do not
construct or interpret one. HTTP cursor serialization is deferred. Provide
constructors/accessors for requests and accessors plus `into_parts()` for
pages.

Semantics:

- empty prefix matches all keys;
- other prefixes match from the beginning of the logical key;
- results use binary lexicographic ordering of key UTF-8 bytes;
- a cursor identifies the inclusive global key position where listing starts;
- the prefix is an independent filter and does not bind the cursor;
- deleting the cursor key continues at the first key after that position;
- LIST returns metadata only and never clones payload bytes;
- pagination is not a frozen snapshot across mutations; and
- an unchanged store never repeats or skips entries.

Each request has a nonzero requested limit. The backend applies:

```text
effective_limit = min(requested_limit, configured_maximum)
```

The cursor references the last object actually returned, so clamping does not
affect continuation. Return no cursor when there is no further match. Detect
that by inspecting at most one match beyond the effective limit rather than
assuming every full page has a successor.

## 7. In-memory configuration and behavior

Add:

```rust
#[derive(Clone, Debug)]
pub struct StoreConfig {
    max_list_page_size: NonZeroUsize,
}
```

The default maximum is 1,000. Expose a constructor and accessor. Preserve:

```rust
Store::new(objects)                    // default config
Store::with_config(config, objects)    // explicit config
```

Duplicate initial keys return `InvalidRequest`. Do not add upload, object-count,
or total-byte limits in this increment.

Use one internal metadata helper for GET, STAT, and LIST. It takes a map key
and stored object and uses checked conversion from `Bytes::len()` to `u64`;
overflow is `Internal`.

Implement operations as follows:

- GET: under a read lock, locate the key, cheaply clone `Bytes`, construct
  metadata, and return `ReadObject`; absence is `NotFound`.
- STAT: under a read lock, construct matching metadata without returning a
  payload handle; absence is `NotFound`.
- LIST: under one read lock, start at the later of the requested prefix and
  cursor position, filter by prefix, clamp the limit, inspect at most one extra
  match, and construct metadata for returned entries only.
- PUT: retain the current request-local context and atomically create or replace
  under one write lock.
- DELETE: remove under one write lock and ignore whether an entry existed.

Successful mutations are visible to later operations through the same store.

## 8. Existing HTTP/2 compatibility

Do not add STAT, LIST, or DELETE routes.

Adapt existing GET:

1. Convert route text into a validated `Key`.
2. Preserve `404` for empty or unknown keys.
3. Call `store.get(&key)`.
4. Map `NotFound` to the existing bounded `404`.
5. Log other storage errors and return the fixed bounded storage `500`.
6. Read content type and length from `ReadObject::metadata()`.
7. Send bytes from `ReadObject::object()` with existing h2 flow control.

Adapt PUT context creation, append, and publication to `StoreError` while
retaining current behavior: success is `200`, invalid request metadata is
bounded `400`, and internal storage failure is bounded `500`. Never return
`StoreError::to_string()` to a peer.

## 9. Documentation alignment

Update `home-object-storage-interface.md`:

- replace CREATE with unconditional create-or-replace PUT;
- use PUT, GET, STAT, LIST, DELETE, and future HEALTH as the operation set;
- remove COPY and logical rename;
- remove two-key locking requirements;
- make DELETE idempotent success without a `removed` boolean;
- retain Conflict only for future conditional operations; and
- distinguish the current key/type/size Rust metadata from later filesystem
  fields such as object ID, checksum, and creation time.

Update `home-object-storage-design.md`:

- remove server-local object copy and logical rename;
- remove security/recovery requirements involving two logical keys;
- replace existing-key conflict language with unconditional PUT replacement;
- remove copy/delete-based rename from preliminary decisions; and
- retain `.part`-to-final filesystem rename because it is physical atomic
  publication, not logical rename.

Link this implementation plan from the roadmap. Keep HEALTH as future work but
do not add it to the Rust trait yet.

## 10. Tests

Metadata and lookup:

- GET and STAT return identical key, content type, and length;
- GET returns the exact payload while STAT exposes no payload;
- missing GET and STAT return `NotFound`;
- metadata updates after replacement; and
- empty and non-empty lengths are exact.

Listing:

- empty and non-empty prefixes;
- binary lexicographic ordering;
- metadata-only results;
- requested limits below and above the backend cap;
- a full final page without a cursor;
- complete unchanged-store traversal without repeats or omissions;
- continuation after deleting the cursor start key;
- reuse of a cursor with a different prefix; and
- empty pages without cursors.

Mutation and errors:

- DELETE present, absent, and repeated;
- atomic PUT creation and replacement;
- `StoreError` kind and local diagnostic display;
- bounded HTTP errors without diagnostic leakage; and
- all existing concurrent GET/PUT and flow-control tests.

Run:

```bash
cargo check -p journey-storage --all-targets
cargo test -p journey-storage
git diff --check
git diff --cached --check
```

Do not run `cargo fmt`, `cargo fmt --check`, `rustfmt`, or another automated
formatter.

## 11. Acceptance criteria

1. The trait supports PUT, GET, STAT, LIST, and DELETE with typed errors.
2. GET returns metadata and payload; STAT returns identical metadata alone.
3. LIST is bounded by backend configuration and uses an opaque global start
   key that is independent of the requested prefix.
4. DELETE is idempotent and does not reveal prior existence.
5. PUT remains atomic unconditional create-or-replace.
6. COPY and logical rename are absent from the planned interface and design.
7. No new HTTP routes are added.
8. Existing HTTP GET and PUT behavior remains compatible.
9. Focused checks and tests pass without automated formatting.
