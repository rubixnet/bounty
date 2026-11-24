# 04: First end-to-end path: an authenticated event reaches SQLite

**What to build:** The first end-to-end path through the daemon. An Adapter holding the per-user bearer token POSTs a valid Traction Event to `/v1/event` on loopback HTTP, gets `202`, and the event is in SQLite after shutdown. A missing or wrong token gets `401`.

This ticket also adds the library-first daemon start/shutdown handle that every later integration test uses. Keep it as one ticket: the black-box test seam needs HTTP and SQLite together. See the daemon decisions (configuration, library-first, token, SQLite, single writer) in `.scratch/step-1a/spec.md`.

**Blocked by:** 02

**Status:** ready-for-agent

- [ ] The daemon config has these settings:
  - data folder, defaulting to `~/.traction`;
  - HTTP port, defaulting to `41937` and accepting `0` for an ephemeral port.
  
  The start function returns a handle that reports the bound address and can trigger a graceful shutdown that flushes, then exits.
- [ ] The listener binds to `127.0.0.1` only. It never binds to `0.0.0.0` or `::`.
- [ ] Token file:
  - on first start the daemon writes 32 CSPRNG bytes, hex-encoded, to `<data dir>/daemon.token`;
  - on Unix the file is created atomically with mode `0600`;
  - later starts reuse the same value;
  - a malformed file is regenerated with a warning that never prints the token.
- [ ] Bearer-token comparison is constant-time.
- [ ] A valid token with a valid event returns `202`. A missing `Authorization` header returns `401`. A wrong token, including one that differs only in the last character, returns `401`.
- [ ] SQLite storage:
  - SQLite is bundled and opened in WAL mode with `synchronous=NORMAL`;
  - the schema is created by a versioned migration (`user_version`);
  - events are stored as validated, typed fields, never as the raw request body.
- [ ] One dedicated writer thread owns the only read-write connection. It receives events over a bounded channel, commits about every 5 seconds, and drains and commits on shutdown.
- [ ] Integration tests:
  - each test starts a real daemon on a fresh temp data folder with port `0`, so tests can run in parallel;
  - tests interact only through HTTP, the token file and a read-only (`mode=ro`) SQLite connection opened after shutdown;
  - N accepted events are all present, and the database reports WAL mode.
- [ ] There is a shared test helper that starts a daemon and returns a client. Later tickets reuse it.
- [ ] Logs never include request bodies or the token.
