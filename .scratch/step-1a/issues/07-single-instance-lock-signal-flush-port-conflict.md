# 07: Single-instance lock, signal flush and port conflict

**What to build:** Only one daemon per user can ever write the database, a crashed daemon never needs manual cleanup, and stopping the daemon loses nothing it already accepted. The binary is a thin wrapper around the daemon library that connects OS signals to the graceful shutdown. See the single-instance lock and port conflict decisions in `.scratch/step-1a/spec.md`.

**Blocked by:** 04

**Status:** resolved

- [x] Lock:
  - Unix uses an exclusive advisory `flock` on a lock file in the data folder;
  - Windows uses a per-user named mutex whose name includes the user and the data folder, so test instances don't collide with a real daemon;
  - the lock is held for the whole process lifetime and released by the OS when the process dies.
- [x] A second start on the same data folder fails with a distinct "already running" error while the first is alive. The binary prints one line and exits non-zero.
- [x] After the first daemon shuts down, a new start on the same data folder succeeds.
- [x] The binary sends `SIGINT`/`SIGTERM` on Unix, and Ctrl+C or console close on Windows, to the graceful shutdown, which flushes pending events before exit.
- [x] If the configured port is already in use, startup fails with a clear error and never falls back to another port.
- [x] Black-box integration tests cover the lock, the restart and the port conflict. The port-conflict test uses a port the test itself occupies.
