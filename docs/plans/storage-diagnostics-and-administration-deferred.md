# Deferred Storage Diagnostics and Administration

**Status:** Deferred  
**Created:** 26 September 2026

The [current object storage design](../design/current-object-storage.md)
describes the implemented object backend and abnormal-event journal. The following
features are separate follow-on work; none is required to start or serve the
filesystem store.

## Later features

- **Integrity command:** Explicitly stream selected payloads and compare
  SHA-256 with the digest in each object header. Report mismatches and I/O
  errors without changing files, the live index, or the HTTP interface. Add
  abnormal findings to stderr and the journal.
- **JSON manifest export:** Produce a consistent, key-sorted snapshot of the
  live in-memory index outside `objects/`. Write a temporary file and replace
  the prior export atomically. The manifest is diagnostic; published object
  files remain the recovery authority.
- **LAN admin page:** Provide a read-only view of object metadata, prefix
  pages, keyed corruption, startup diagnostics, and filesystem capacity. Keep
  it separate from the public object routes and restrict access to the home
  network.

Before implementing these features, settle the command interface and output
for integrity checks, the manifest format and export trigger, and the admin
listener, routes, and authentication. The older
[storage interface design](../design/home-object-storage-interface.md) offers background
but still describes SQLite and is not the implementation contract.
