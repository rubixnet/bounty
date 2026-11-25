# The Unix socket skips only the `Host` check

The Local Ingestion Bus runs one check pipeline on both transports, loopback HTTP and the Unix Domain Socket (macOS/Linux). Requests arriving on the UDS skip the `Host` allowlist and nothing else. The `Host` check exists to defeat DNS rebinding: a website that re-points its own hostname at `127.0.0.1` can make a browser send requests to the loopback port, and the foreign `Host` header gives it away. A browser cannot open a Unix socket, so a UDS request has no TCP origin and DNS rebinding cannot reach it. The socket file is also mode `0600`, so only the owning user can connect at all. Requiring a loopback `Host` on the UDS would protect nothing, and it would force every Adapter to invent a fake `Host` value for a transport that has no host. We considered applying the identical pipeline to both transports for simplicity and rejected it for that reason.

## Consequences

- The `Origin` allowlist and the bearer token still apply to every UDS request, so both transports share one security model for credentials and browser-originated input.
- The body cap, validation and writer path are identical on both transports.
- A UDS request may carry any `Host` header, or none, and is not refused for it.
- If the socket's file mode or location ever becomes reachable by other users, this exemption must be revisited.
