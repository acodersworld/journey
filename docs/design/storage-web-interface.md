# Storage Web Interface

**Status:** Implemented in `journey-storage`

**Scope:** Browser object manager for the existing storage backends

## Server and routes

`journey-storage` exports `serve_web_interface`, which accepts an already
bound Tokio TCP listener, an `Arc<S>` where `S: StoreInterface`, and validated
`WebCredentials`. It builds an Axum HTTP router and calls the store directly.
The HTTP/2 `Service` also accepts an `Arc<S>`, so callers can clone the `Arc`
to share one backend between both listeners. The in-memory and filesystem
backends themselves are not clonable.

The web listener serves an embedded object manager and these routes:

| Route | Behavior |
| --- | --- |
| `GET /` | Embedded browser interface |
| `GET /api/entries?prefix=&cursor=` | Up to 100 immediate folders and objects |
| `GET /api/metadata?key=` | Key, content type, and payload size |
| `GET /api/download?key=` | Stream an object as an attachment |
| `PUT /api/object?key=<key>` | Stream a raw file body into a caller-keyed PUT |
| `PUT /api/object?prefix=<prefix>` | Stream a raw file body into a generated-key PUT |
| `DELETE /api/object?key=` | Delete an object |

The public `StoreInterface` and the existing h2c routes are unchanged. The
Python request console remains an independent development tool.

## Authentication and request checks

Every matched page or API route requires HTTP Basic authentication. Credentials
must have a nonempty username and password, and the decoded username and
password are compared through fixed-size SHA-256 digests using a constant-time
equality check. Failed requests receive `401` and `WWW-Authenticate`.
Responses use `Cache-Control: no-store`. The interface sets no cookies and
does not enable CORS.

PUT and DELETE also require an `Origin` that matches the request's HTTP host
and port. PUT accepts only `If-None-Match: *` (create-only) or
`If-Match: *` (replace-only). Storage errors have bounded response bodies;
internal details are logged locally.

An upload supplies exactly one of `key` or `prefix`. A caller-keyed upload
uses `key` and cannot include `Object-Key-Mode`. A generated upload supplies
`prefix` with exactly one `Object-Key-Mode: sha256` header; an empty prefix
stores at the root, while a nonempty prefix must end in `/` and leave room for
the 64-character digest within the 1,024-byte key limit. The browser checkbox
uses the current folder as that prefix. Successful generated uploads return
the full key as JSON; caller-keyed uploads retain the `204 No Content`
response.

The example binds its web listener to `0.0.0.0:8082` by default and uses the
example-only credentials `user` / `pass`. `--web-bind` overrides that address;
`--bind` configures the h2c listener. The browser interface uses plain HTTP, so its Basic
credentials and traffic are unencrypted. The separate h2c listener has its
own access boundary.

## Folder derivation and pagination

Folders are virtual prefixes derived from healthy keys returned by
`StoreInterface::list`; there are no empty folder objects or create-folder
operation. A prefix is empty at the root or ends in `/`. Keys are processed in
lexicographic order with one-item LIST requests.

An object without another slash becomes an object row. A key with another
slash becomes one immediate folder row. After an object, listing resumes at
the store's returned cursor. After a folder, the final slash in its prefix is
replaced with `0` to seek inclusively beyond that subtree. The server returns
at most 100 visible rows and a Base64URL next seek key only when another row
exists. This bounds memory and prevents large folders from repeating on later
pages. A trailing slash is reserved for virtual folder prefixes, so no object
key can equal a folder prefix or end in `/`.

## Streaming and browser behavior

PUT consumes request body chunks sequentially and appends each chunk to a
store PUT context. Download reads fixed-size chunks from `ObjectInterface`
only as the response body is polled, allowing backpressure and cancellation.
It reads no more than the stored payload length, ends without another reader
call at that length, and fails the body stream on early EOF, invalid read
counts, or reader errors. Downloads use the stored content type and payload
length and carry an attachment disposition.

The page uses same-origin browser requests and encodes full logical keys in
query values. Each listing request has an increasing ID; only the latest
request may update the rows, pagination, or status after its response arrives.
It displays folders, metadata, downloads, uploads, and deletion.
Uploads use the selected file's type or `application/octet-stream`, with
create-only as the default. A `412` response prompts before a second
replace-only upload; dismissal sends no replacement. Deletion also requires
browser confirmation. Returned key and metadata strings are rendered with
`textContent`.
