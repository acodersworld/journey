# Site database backup to object storage

## Goal

Keep a recoverable copy of the site SQLite database in the object store. The
site creates a consistent snapshot while serving requests, compresses it, and
replaces a single backup object. The same operation is available on demand.

## Backup creation

- Add a site backup component shared by the daily scheduler and the private
  control socket. Serialize backup jobs so a manual request cannot overlap a
  scheduled one.
- Use SQLite's Online Backup API to copy the live database into a temporary
  SQLite file. Do not copy the live database file directly, since it can change
  while the copy is in progress. Perform blocking SQLite and compression work
  off the async runtime.
- Check the snapshot with SQLite `PRAGMA integrity_check` and
  `PRAGMA foreign_key_check`, and require the current site schema version.
  Compress the validated snapshot with Zstandard into another temporary file.
  Clean up both temporary files after success or failure.
- Extend the site storage client with an explicit-key file PUT, using the
  existing H2C and WebSocket transports. Upload with content type
  `application/zstd` to `backups/site/latest.sqlite3.zst`. Use the object
  service's atomic replacement behavior so an incomplete upload cannot replace
  the last successful backup. Treat the storage success response as the end of
  the backup job.
- Keep backup keys outside public media routes. Do not add a public HTTP backup
  endpoint or a database row referring to the backup.
- Log start, completion, and failure, including the object key and useful
  error context. A failed job must not stop the site.

## Scheduling and commands

- Add `[backup] daily_utc = "03:00"` to site configuration, with `03:00` UTC as
  the default. Accept a valid 24-hour `HH:MM` value and reject malformed
  values at startup.
- The running site triggers one backup at the configured UTC time each day.
  Schedule the next future occurrence when it starts; skip runs missed while
  the site was stopped. If a scheduled run fails, log it and wait for the next
  scheduled day unless an operator triggers a manual retry.
- Add `journey-site backup create`. It requests a backup from the running site
  over the existing private Unix control socket, so WebSocket deployments use
  the site's established storage connection. Return a nonzero exit status on
  failure. Extend the control protocol with an explicit command discriminator
  while retaining the existing import command.
- Add `journey-site backup restore-check <output-db>`. Through the running
  site's control socket, download the fixed backup object, decompress it into
  a temporary file, check SQLite integrity, foreign keys, and schema version,
  then atomically publish the validated database at an absolute, container-
  visible output path. Refuse to replace any existing output file, including
  the live database. The operator stops the site and installs the validated
  file manually.
- Add `journey-site backup verify-file <input.zst> <output-db>` for recovery
  when the site cannot start. This command works without initializing the live
  database or connecting to the control socket. It performs the same
  decompression and validation on a backup downloaded directly from home
  object storage, then writes a new validated output file.
- Write output files with private permissions. Remove partial output files on
  errors. Keep all backup and restore operations local to CLI/control access;
  do not add browser routes.

## Documentation

Document the backup configuration and commands in the site deployment guide,
including the fixed object key and the stopped-site restore procedure. Update
the architecture design's database backup section: the initial implementation
keeps only the latest snapshot, uses no application-level encryption, and does
not use backup generations or automatic restore. Note that the object contains
account and session data and that replacing the sole backup loses older states.

## Verification

- Create and restore-check a backup while the site continues writing to the
  database. Verify the restored database passes SQLite checks and has the
  expected schema version.
- Run two backups and verify the fixed object key holds only the latest one.
  Simulate an upload failure and verify the previous object remains usable.
- Verify manual backup works through the control socket with both H2C and
  WebSocket storage transports, and that the scheduler runs once at the chosen
  UTC time without catching up after downtime.
- Reject missing, truncated, invalid, and wrong-schema backups without
  replacing an existing output file. Verify offline `verify-file` works when
  the live site database is unavailable.
- Verify the backup object is not reachable through the public media route.

## Assumptions

- The backup is Zstandard-compressed but not encrypted by the application.
- Only the latest backup is retained. This deliberately provides no history
  for undoing older accidental changes.
- No site database schema change or data migration is required.
