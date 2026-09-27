# Built-In Storage Web Interface Implementation Plan

**Status:** Ready for implementation  
**Created:** 27 September 2026  
**Scope:** An S3-style object browser within `journey-storage`

## 1. Goal and boundary

Add a browser-compatible HTTP listener to the storage crate. It uses the same
`StoreInterface` instance as the existing HTTP/2 service, calling the store
directly rather than proxying through `/objects`. Keep the HTTP/2 object routes
and the public storage trait unchanged. This is an object manager, separate
from the deferred read-only diagnostics and LAN administration page.

The first interface shows slash-separated key prefixes as folders and supports
listing, metadata inspection, download, upload, and deletion. Folders are
derived from existing object keys; there is no empty-folder object or create
folder operation. Keep the existing Python request console as a separate tool.

## 2. Library server and example

- Add a crate-owned web UI module and embedded HTML/CSS/JavaScript asset. Use
  Axum for browser-compatible HTTP; it is already a workspace dependency.
  Export a reusable async server entry point accepting a bound `TcpListener`,
  an owned `S: StoreInterface`, and a credentials value. The server owns its
  router and calls the store directly. The caller can clone either current
  store backend to share it with `Service<S>`.
- Require nonempty configurable username and password for the library server.
  Protect the page and every data route with HTTP Basic authentication, compare
  decoded credentials in constant time, and return `401` with
  `WWW-Authenticate` on failure. Use no cookies or CORS. For PUT and DELETE,
  require an `Origin` matching the request's HTTP origin.
- Have `h2c_get_server` start the web listener automatically, without a flag,
  alongside its existing h2c listener. Default its bind to `0.0.0.0:8082`,
  allow `JOURNEY_STORAGE_WEB_BIND` to override it, and use the example-only
  credentials `user` / `pass`. Fail startup clearly if either listener cannot
  bind; report both addresses. Keep the h2c bind configuration separate.
- Change the Python request console's default port to `127.0.0.1:8083` and
  update its run instructions. The new browser UI must not depend on Python or
  HTTPX.

The example deliberately uses Basic authentication over plain LAN HTTP, as
requested. Credentials and traffic are unencrypted. Authentication on the web
listener does not protect the separate h2c listener.

## 3. Browser routes and behavior

All routes below belong to the web listener, not the existing HTTP/2 object
API. Authenticate them before serving content or reaching the store.

| Route | Behavior |
| --- | --- |
| `GET /` | Embedded object browser page. |
| `GET /api/entries?prefix=&cursor=` | One page of immediate folders and objects under the prefix. |
| `GET /api/metadata?key=` | Key, content type, and payload size from `stat`. |
| `GET /api/download?key=` | Stream an object as an attachment. |
| `PUT /api/object?key=` | Stream a file into a PUT context; accept only create-only or replace-only condition headers. |
| `DELETE /api/object?key=` | Delete after the browser confirms. |

The page shows breadcrumb navigation, a paginated table of immediate folders
and objects, object size and content type, and controls for metadata, download,
upload, and delete. Use `textContent` for returned keys and metadata rather
than inserting them as HTML. Encode complete logical keys in browser URLs.
Show a direct object row if an object key equals the current folder prefix, so
keys ending in `/` remain accessible. Do not hide keys containing repeated
slashes or other valid characters.

When a file is selected, suggest the current prefix plus its filename as the
key, and allow the user to edit it. Use the browser-reported file type or
`application/octet-stream`. Upload the `File` as the raw request body and
create-only by default. On `412 Precondition Failed`, ask whether to replace;
if confirmed, send the same file again with replace-only. Never replace on a
failed or dismissed confirmation. Confirm deletion before sending DELETE.
Surface bounded errors in the page without exposing internal store details.

Stream request chunks into `PutContextInterface::append` and stream download
chunks from `ObjectInterface::read`; neither handler collects a complete file.
Honor backpressure and stop work when the browser disconnects. Use the
storage metadata for download `Content-Type` and `Content-Length`, and send
`Content-Disposition: attachment`.

## 4. Folder listing and pagination

The web server builds folder pages from `StoreInterface::list` without adding
a delimiter argument to the trait or calling the HTTP/2 LIST route. A prefix
is either empty for the root or ends in `/`. For each healthy key returned in
lexicographic order, remove the current prefix. If the remainder contains
`/`, emit one folder row for its first component; otherwise emit the object.
The existing LIST behavior omits corrupt entries, so the browser does too.

Use bounded, one-item LIST requests to seek through the ordered index. After
an object, continue at the returned LIST cursor. After a folder, skip its
whole subtree by replacing the final `/` in that folder prefix with `0` and
using the resulting string as the next inclusive seek key. This is a lower
bound beyond every key beginning with the folder prefix. Return at
most 100 visible rows and a Base64URL-encoded next seek key when another row
exists. Validate incoming cursors as UTF-8 keys within the requested prefix.
This keeps memory bounded and prevents a folder from appearing on multiple
pages even when it contains thousands of objects. A later storage-specific
optimization may replace the repeated LIST calls without changing UI behavior.

## 5. Verification

- Test that every route requires Basic credentials; wrong credentials return
  `401`, and cross-origin PUT and DELETE are rejected. The existing h2c API
  behavior and bind setting remain unchanged.
- Test root and nested folder pages with more than 100 immediate entries,
  thousands of keys in one folder, adjacent object and folder names, an
  object whose key ends in `/`, repeated slashes, UTF-8, and URL-special
  characters. No folder or object is duplicated or skipped across pages.
- Test metadata, byte-exact downloads, create-only upload, the `412` prompt
  and confirmed replace-only retry, cancelled replacement, and confirmed
  deletion against in-memory and filesystem stores.
- Test multi-chunk upload and download paths so handlers do not buffer whole
  files. Run `cargo test -p journey-storage` and `git diff --check`. Do not run
  a source formatter.
