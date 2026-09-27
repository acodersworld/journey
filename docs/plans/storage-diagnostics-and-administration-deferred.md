# Deferred Storage Diagnostics and Administration

**Status:** Deferred  
**Created:** 26 September 2026

**Updated:** 27 September 2026

The [current object storage design](../design/current-object-storage.md)
describes the implemented filesystem backend and abnormal-event journal. The
[storage web interface](../design/storage-web-interface.md) already provides
authenticated object listing, metadata, download, upload, and deletion. The
features below remain follow-on work; none is required to serve the store.

## Later features

- **Integrity command:** Explicitly stream selected payloads and compare
  SHA-256 with the digest in each object header. Report mismatches and I/O
  errors without changing files, the live index, or the HTTP interface. Add
  abnormal findings to stderr and the journal.
- **JSON manifest export:** Produce a consistent, key-sorted snapshot of the
  live in-memory index outside `objects/`. Write a temporary file and replace
  the prior export atomically. The manifest is diagnostic; published object
  files remain the recovery authority.
- **Diagnostic web view:** Expose keyed corruption, startup diagnostics, and
  filesystem capacity to an administrator. The existing browser interface
  already covers healthy object metadata and prefix pages. Keep diagnostic
  information off the unauthenticated HTTP/2 object routes. Decide whether
  to extend the authenticated browser interface or use a separate admin
  listener when planning this feature.

Before implementation, settle the command interface and output for integrity
checks, the manifest format and export trigger, and the diagnostic view's
data source and access policy. The older
[storage interface design](../design/home-object-storage-interface.md) offers
background but still describes SQLite and is not the implementation contract.
