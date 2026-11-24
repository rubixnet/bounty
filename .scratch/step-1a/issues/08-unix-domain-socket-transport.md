# 08: Unix-socket transport (macOS/Linux)

**What to build:** On macOS and Linux, local Adapters can reach the Local Ingestion Bus through a socket that only the user can open, without touching TCP. The socket has the same security model as HTTP. Windows stays HTTP-only in 1a. See the Local Ingestion Bus decision in `.scratch/step-1a/spec.md`.

**Blocked by:** 05

**Status:** ready-for-agent

- [ ] The socket path is configurable. It defaults to `$XDG_RUNTIME_DIR/traction.sock`, and falls back to `<data dir>/daemon.sock` when `XDG_RUNTIME_DIR` is unset.
- [ ] The socket file mode is `0600`. A stale socket file left by a dead daemon is safely replaced only while the single-instance lock is held.
- [ ] `POST /v1/event` over the socket uses the same token, body-size cap, validation and writer path as HTTP. It skips only the `Host` check.
- [ ] Unix-only integration tests:
  - a valid request returns `202` and the event is persisted;
  - a bad token returns `401`;
  - the socket file mode is `0600`.
- [ ] The daemon still builds and all tests pass on Windows, where the socket code is compiled out.
