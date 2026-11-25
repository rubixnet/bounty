# 08: Hook-coverage dedup, hook-reported `transcript_path`, paused-span drops

**What to build:** Hooks and the tailer work together without double-counting. A session delivered both as hooks and as a transcript yields exactly one set of events, while spans the hooks missed (the daemon was down, hooks never authenticated, records before a run's first hook) are filled in from the transcript. The tailer prefers a hook-reported `transcript_path`, but only inside the Claude projects folder, and never records anything from a paused span. See spec "Hook coverage (dedup rule)" and "Pause".

**Blocked by:** 04, 07

**Status:** ready-for-agent

- [ ] New migrations add `daemon_run (run_id, started_ms, last_alive_ms)`, where `last_alive_ms` is bumped on every writer commit, and `session_hook_run (harness_id, session_id, run_id, first_hook_ms)`, set on a session's first accepted hook event in a run.
- [ ] A tailed record is dropped when its timestamp falls in `[first_hook_ms, last_alive_ms]` of any run for its session (the current run is open-ended). A tailed `SESSION_START` is dropped when one is already stored for that session.
- [ ] A `transcript_path` from any hook is remembered for that session only if it canonicalizes to a file under `<claude config dir>/projects/`. Anything else is ignored and never opened.
- [ ] The tailer drops records whose timestamp falls in a paused span of their session.
- [ ] Seam A tests cover: hooks + transcript for the same session giving one set of events; a "daemon down" span filled from the transcript (stop the daemon, append, restart); a record about a second from a coverage boundary (clock skew); a forged `transcript_path` ignored; paused-span records not stored.
- [ ] `docs/protocol.md` documents the dedup rule and the hook-coverage assumption from the spec's Further Notes.
