# Outbound WebSocket idle heartbeat

Only WebSockets opened by `journey-websocket` initiate idle heartbeats. The
connection direction is carried into the HTTP/2 session independently of the
inner HTTP/2 client or server role. Accepted WebSockets do not send periodic
Pings, but both sides answer peer initiated WebSocket Pings.

An outbound connection sends a WebSocket Ping after 30 seconds without nonempty
binary messages carrying HTTP/2 bytes in either direction. The interval and the
10 second Pong timeout are configurable through `Config` and must be positive.
Each Ping carries a monotonically increasing payload. The matching WebSocket
Pong must arrive within the timeout; other Pongs and binary traffic do not
complete an outstanding heartbeat. A stalled Ping send is bounded by the same
timeout. Missing Pongs and failed sends terminate the bridge so the application
can reconnect.

Heartbeat control traffic stays in the WebSocket transport. It does not use
HTTP/2 PING frames or application requests.
