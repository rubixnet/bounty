# 08: Unix-socket transport (macOS/Linux)

**What to build:** On macOS and Linux, local Adapters can reach the Local Ingestion Bus through a socket that only the user can open, without touching TCP. The socket has the same security model as HTTP. Windows stays HTTP-only in 1a. See the Local Ingestion Bus decision in `.scratch/step-1a/spec.md`.

**Blocked by:** 05

**Status:** resolved

- [x] The socket path is configurable. It defaults to `$XDG_RUNTIME_DIR/traction.sock`, and falls back to `<data dir>/daemon.sock` when `XDG_RUNTIME_DIR` is unset.
- [x] The socket file mode is `0600`. A stale socket file left by a dead daemon is safely replaced only while the single-instance lock is held.
- [x] `POST /v1/event` over the socket uses the same token, body-size cap, validation and writer path as HTTP. It skips only the `Host` check.
- [x] Unix-only integration tests:
  - a valid request returns `202` and the event is persisted;
  - a bad token returns `401`;
  - the socket file mode is `0600`.
- [x] The daemon still builds and all tests pass on Windows, where the socket code is compiled out.

## Comments

- **Scope extra: `--socket-path` CLI flag (Unix only).** Alongside ticket 07's `--data-dir` and `--port`, the binary accepts `--socket-path <path>`. Why: story 43 wants the socket path configurable. The binary tests pass it (or set a temp `XDG_RUNTIME_DIR` on the child only) so parallel tests never bind the real `$XDG_RUNTIME_DIR/traction.sock` and collide with `SocketInUse`. Without the flag the default is `$XDG_RUNTIME_DIR/traction.sock`, else `<data dir>/daemon.sock`.
- The socket's `Host` exemption is recorded in `docs/adr/0003-uds-skips-host-check.md`.
