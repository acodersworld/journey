# h2c storage browser console

## Purpose

The h2c example accepts HTTP/2 prior-knowledge connections, which browsers do
not initiate over plain HTTP. This example adds a loopback-only HTTP/1 browser
server that serves a request console and forwards storage requests to the
existing h2c listener. The Rust storage service, its routes, and its in-memory
state are unchanged. Run both processes; stopping the Rust process loses
uploaded objects.

## Files and dependencies

- `h2c_web_console.py` serves the page and proxies requests.
- `h2c_web_console.html` contains the browser controls and response viewer.
- Python 3.9 or newer and `httpx[http2]` are required. HTTPX's HTTP/2 extra
  installs the HTTP/2 protocol dependencies.

The Python server uses `ThreadingHTTPServer` and a shared `httpx.Client` with
`http1=False`, `http2=True`, and `trust_env=False`. Disabling HTTP/1 forces
prior-knowledge h2c on the cleartext upstream connection; disabling environment
proxy settings keeps requests on the configured local connection. The HTML page
uses ordinary same-origin `fetch` requests to the Python server, so the browser
does not need h2c support or cross-origin access.

## Routes and behavior

| Browser route | Upstream route | Console operation |
| --- | --- | --- |
| `GET /` | None | Serve the HTML console. |
| `GET /api/objects?prefix=...&limit=...&cursor=...` | `GET /objects?...` | List keys and follow `next_cursor`. |
| `GET /api/objects/<key>` | `GET /objects/<key>` | Show text or JSON, or download binary bytes. |
| `HEAD /api/objects/<key>` | `HEAD /objects/<key>` | Inspect status and metadata headers. |
| `PUT /api/objects/<key>` | `PUT /objects/<key>` | Upload or replace bytes with the chosen `Content-Type`. |
| `DELETE /api/objects/<key>` | `DELETE /objects/<key>` | Delete a key after browser confirmation. |
| `POST /api/objects` | `POST /objects` | Exercise the service's `405 Method Not Allowed` response. |

The proxy fixes the upstream origin at process startup. It passes through the
object path suffix and listing query, and never accepts an upstream URL from a
browser request. In the key field, enter the URI path suffix exactly as it
should appear after `/objects/`; use percent escapes for special characters.
The listing controls encode query values with percent escapes, including spaces
and literal plus signs. The `prefix` parameter is always present, even when it
is empty; `limit` and `cursor` are omitted when their fields are empty.

PUT sends the selected file's bytes, or UTF-8 text when no file is selected,
as the entire request body. It does not use multipart encoding. Clearing the
content-type field sends no `Content-Type`, allowing the storage service to
return its normal validation error. Empty uploads are supported.

The response viewer shows status, upstream HTTP version, response headers,
and text or JSON bodies. It offers binary GET responses as downloads and
truncates displayed text after 64 KiB. The browser retains a binary response
in memory while its download link is available. The proxy streams upstream
response bytes to the browser, forwards `Content-Type`, `Content-Length`, and
`Allow`, and retains upstream error statuses and bodies. A connection failure
becomes `502`; an upstream timeout becomes `504`. Neither condition stops the
browser server.

This is a local development tool. It has no authentication, upload quota, or
persistence; the underlying example also retains entire uploads in memory.
Keep both listeners bound to loopback unless those limits are addressed.

## Run

From the workspace root, start the storage example:

```bash
cargo run -p journey-storage --example h2c_get_server
```

In another terminal, install the Python dependency in a virtual environment
and start the console:

```bash
python3 -m venv /tmp/journey-h2c-console-venv
/tmp/journey-h2c-console-venv/bin/python -m pip install 'httpx[http2]'
/tmp/journey-h2c-console-venv/bin/python crates/journey-storage/examples/h2c_web_console.py
```

Open `http://127.0.0.1:8082/`. The defaults match the Rust example's
`127.0.0.1:8081` listener. To use other loopback ports, set
`JOURNEY_STORAGE_BIND` for the Rust example, then pass `--storage-url
http://127.0.0.1:<port>` and optionally `--listen 127.0.0.1:<port>` to the
Python script. Stop each process with Ctrl-C.

## Acceptance checks

1. List with empty prefix and confirm `image.jpg` and `video.mp4`. Set limit
   to `1`, click **Next page**, and confirm the second page differs.
2. GET and download each fixture. Compare the downloaded bytes with the
   corresponding file in `examples/assets/`.
3. HEAD `image.jpg` and confirm `200`, `Content-Type: image/jpeg`, its byte
   length, and an empty body. HEAD a missing key and confirm `404`.
4. PUT a new key using a file and a content type; GET it and compare bytes.
   PUT different bytes and a different content type to the same key, then
   confirm GET and HEAD report the replacement.
5. DELETE the key and confirm `204`. Repeat DELETE and confirm the same
   status. GET it and confirm `404`.
6. Clear the PUT content type and confirm the upstream `400` is visible.
   Select the POST collection check and confirm `405` and `Allow: GET`.
   Stop the Rust server and confirm the console reports `502` without exiting.

For automated checks, run the Python script against a separately launched
Rust example and request the proxy routes with an HTTP/1 client. Assert the
`X-Upstream-HTTP-Version: HTTP/2` response header and compare exact fixture
bytes. Keep the browser page in the manual checks because it exercises file
selection, download links, and cursor handling.
