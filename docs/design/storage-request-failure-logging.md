# Storage request failure logging

Storage request errors include the HTTP method, URI path, and HTTP/2 stream ID.
The path is formatted with debug escaping, and request queries and headers are
excluded so credentials are not copied into failure logs.

A GET failure is routine only when h2 confirms that the remote peer reset the
stream with `CANCEL`. Closed streams without a reset reason and all other h2
errors remain visible. PUT failures remain visible even when the peer reset
their stream with `CANCEL`.
