# Storage Web Interface Review Fixes

**Status:** Planned

## Goal

Keep the browser listing consistent when requests overlap, and ensure a web
download delivers exactly the payload length reported by storage metadata.

## Implementation

- In `storage_web_interface.html`, give each `loadPage` call a monotonically
  increasing request ID. After the response body is parsed, and in the error
  path, update rows, pagination controls, and status only if that ID is still
  current. This covers folder navigation, Next/Previous, and reloads after
  upload or delete. A newer request owns the display even if an older one
  completes later.
- In `storage_web_interface.rs`, pass the metadata payload length to
  `object_stream`. Track remaining bytes, request at most the remaining
  length in each read, and finish without calling the reader when it reaches
  zero. Treat `Ok(0)` before that point or a count larger than the supplied
  buffer as a stream error. Log the key and read failure to stderr, while
  keeping storage details out of the HTTP response. Preserve streaming and
  backpressure; do not buffer the whole object.
- Keep the existing public web function and storage traits unchanged.

## Verification

- Delay an earlier listing response, navigate to another folder or page,
  then release it. The display must retain the newer request's entries,
  breadcrumbs, pagination, and status. Check that an older failed request
  cannot replace a newer success message.
- Test web downloads with readers that return short positive reads, early
  `Ok(0)`, and a count larger than the offered buffer. Successful downloads
  contain exactly the declared bytes; malformed reads fail the body stream.
  A zero-length object completes without reading its reader.
- Run the journey-storage test suite without source formatters.
