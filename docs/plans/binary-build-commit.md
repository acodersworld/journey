# Report the Git commit embedded in deployed binaries

## Summary

Add a build commit to `journey-site` and `journey-storage-service`. Retrieve it
with `--version` or `-V`, including inside a running container. The output
includes the full commit hash and a `dirty` marker when the build used
uncommitted changes.

## Implementation

- Add build scripts to both binary packages. Each accepts build metadata
  supplied by the build command; otherwise it reads the commit and working-tree
  state from the local Git checkout. Embed the result at compile time, with no
  runtime Git dependency. Report `git: unknown` when neither source is
  available.
- Extend the site's existing Clap version output. Add top-level `--version`
  and `-V` handling to storage before it parses a role or loads configuration.
  Use the same output format for both binaries.
- Pass the host commit and dirty state into the root Docker build and both
  app-specific Docker builds. Update the documented build commands accordingly;
  Docker build contexts exclude `.git`.
- Ensure changes to the Git ref and relevant source state cause Cargo to
  refresh embedded metadata on a local rebuild.

## Verification

- Build locally and check both version commands against `git rev-parse HEAD`.
- Check that uncommitted changes produce the `dirty` marker.
- Build a Docker image and check both executables inside it without Git
  installed.
- Build from a source tree without Git metadata or supplied values and check
  for `git: unknown`.

## Assumptions

- This covers the two deployed binaries; the gateway and home validation
  binaries are outside this change.
- The commit is available through the CLI only. No HTTP endpoint is added.
