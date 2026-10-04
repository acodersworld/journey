# Site and storage shutdown

The `journey-site serve` and `journey-storage-service` processes treat SIGINT
and SIGTERM as successful shutdown requests. Each process broadcasts one signal
to its listener and session tasks so they stop accepting work together.

Shutdown cancels active HTTP requests, storage requests, imports, uploads, and
downloads instead of draining them. The site closes its active storage
connection, including the direct h2c client session, and removes the private
import control socket it created. The storage service stops its management UI
and drops the active HTTP/2 session before exiting. An interrupted unpublished
PUT is discarded with its request context; it is not resumed after restart.

Listener failures during normal operation are process errors. Task cancellation
caused by a shutdown request is expected and is not reported as a listener or
request failure. Normal storage reconnect attempts continue until shutdown is
requested.
