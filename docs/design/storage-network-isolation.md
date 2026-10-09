# Storage network isolation

The deployed object store runs in a `storage` process with no IP network. A
small `connector` owns the outbound WebSocket and bridges its binary messages to
the storage process over one Unix stream. A separate `management-proxy` relays
loopback TCP clients to the authenticated management router over another Unix
socket.

```text
site WSS <-> connector <-> storage-session.sock <-> storage (network_mode: none)
operator <-> SSH tunnel <-> management-proxy <-> management.sock ------------^
```

The three roles use one executable and image, with separate configuration
files and volume mounts:

| Role | Network | Access |
| --- | --- | --- |
| `storage` | No IP network | Object volume, session socket, management socket, management credentials |
| `connector` | Outbound IP network | Session socket and site URL/WebSocket secret |
| `management-proxy` | TCP listener for the published loopback port | Management socket only |

Storage keeps the filesystem store, HTTP/2 request handlers, FFmpeg work,
thumbnail cache, and authenticated management UI. The filesystem store and
thumbnail cache remain alive across connector reconnects. FFmpeg retains its
existing limit of two concurrent media jobs.

The connector does not inspect HTTP/2 requests and does not accept a destination
address or dial command from the stream. It connects only to the configured
site URL. WebSocket binary message boundaries are removed when bytes enter the
Unix stream; bounded chunks from the stream become binary messages. WebSocket
Ping/Pong, the negotiated subprotocol, frame limits, heartbeat, backpressure,
close timeout, and error propagation remain in the transport crate. One Unix
connection carries one HTTP/2 session. Closing either side ends that session;
requests in flight fail and are not replayed.

Storage serves at most one active session. During the HTTP/2 handshake and
while it serves that session, it accepts and closes any extra local connection
instead of allowing it to wait and replace the active connector later. The
handshake has a 15-second limit. When a session ends, request handler tasks are
cancelled. An incomplete PUT remains unpublished and its temporary file is
removed when its context is dropped.

The session and management sockets live in separate named volumes. Both
directories are owned by UID/GID 65532 and checked for ownership and access at
startup. Storage holds an exclusive lock file in the session directory for the
lifetime of both listeners. It accepts only the lock file and the two expected
socket names in those directories. Before binding, it checks each expected
existing path is a socket with no live listener, then removes that stale socket
path. It rejects non-socket paths, live listeners, and unexpected directory
entries. The persistent lock pathname itself is retained between runs.

The management proxy has no credentials, object volume, or session socket. The
management UI's Basic authentication stays in storage. Compose publishes the
proxy only at `127.0.0.1` on the storage host; remote access uses SSH forwarding.
The connector has no object volume or management socket. The root and storage
Dockerfiles create both socket mount points with the correct UID/GID and
private directory mode so new named volumes inherit usable ownership.

## Security boundary

This split protects the home LAN from network access by object parsing and
media processing in the storage process. A compromised website host still has
the existing storage API and can read, list, write, delete, or exfiltrate
objects. The connector parses TLS and WebSocket traffic while networked; a
connector compromise can still pivot through routes available to that
container. Restrict connector egress at the host or network layer if that
remaining risk needs a tighter boundary.

The `all` command is a local development launcher. It writes private temporary
role configuration files, creates private temporary socket directories, waits
for the storage sockets and management proxy, then starts the three roles as
children. It removes the temporary files and reaps the children on shutdown.
Its child processes inherit the developer machine's network access and provide
no network isolation.
