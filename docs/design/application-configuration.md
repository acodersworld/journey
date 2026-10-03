# Application configuration

The `journey-site` and `journey-storage-service` binaries read a TOML file
selected with `--config <PATH>` or the `JOURNEY_CONFIG` environment variable.
The selected path is the only application setting passed through the
environment. The binaries do not read individual settings from environment
variables.

Site and storage processes use separate files because they run on different
hosts in the LAN deployment. `deploy/site.toml.example` and
`deploy/storage.toml.example` document the available settings. The site file
contains the shared WebSocket secret; the storage file contains the same value
plus the object path and management credentials. Both should be kept private
and mounted read-only in containers.

Configuration types reject unknown keys and validate addresses, origins,
credentials, and HTTP/2 window sizes before opening persistent storage or
binding service listeners. CLI options such as `--window-size` override the
corresponding TOML values for one invocation.

Relative database, control-socket, and object-directory paths are resolved
relative to the selected configuration file. Absolute paths are recommended
for container deployments.

Docker Compose uses `JOURNEY_CONFIG` only to point each service at its mounted
file. Compose-level values such as image tags, host port bindings, and the
import directory remain in the Compose `.env` file because Compose itself uses
them to create mounts and publish ports.
