# Binary build commit

The `journey-site` and `journey-storage-service` binaries include a build
version in `--version` output. Both print that same version when the service
starts. The version has the package version and a Git commit value, for
example `0.1.0 (git: 0123456789abcdef...)`.

Docker builds use `deploy/build-image.sh`, which makes a temporary depth-one
clone of local `HEAD` and supplies its full hash as the `BUILD_COMMIT` build
argument. Its `all` mode builds separate site and storage images from the same
clone. Uncommitted changes are excluded. Cargo builds without that argument
use `git: unknown`; only the host-side helper needs the Git executable, and
the Cargo build scripts and running binaries use no Git crate.
