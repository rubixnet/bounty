# 08: Attribution windows (hook, log-tailed turn, trailing fallback, precedence, Project scoping)

**What to build:** In-memory, never persisted attribution windows fed by accepted events decide each held `FILE_MUTATION`: hook window (`TOOL_PRE`→`TOOL_POST`/`TOOL_FAIL` inside an open turn, `harness_hook`/`harness_plugin`), log-tailed turn window, trailing fallback (`LOG_TAIL_TRAILING_WINDOW` = 30 s) for log-tailed sessions with no turn boundaries; compared on event timestamps; scoped to the session's current Project; precedence hook > log turn > trailing, most recently opened wins; winner's harness/session/network_class copied, else manual. See spec "Attribution windows (daemon, in memory, fed by accepted events)" and Seam 2 "Attribution".

**Blocked by:** 07

**Status:** ready-for-agent

- [ ] Hook window attributes; edit after `TOOL_POST` or `TURN_END` is manual.
- [ ] Log-tailed turn attributes; after `TURN_END` manual; late `PROMPT_SUBMIT` within hold claims, after hold does not.
- [ ] Trailing fallback for `harness_db_poll` session: within window attributed, beyond manual (test override keeps the ratio).
- [ ] Project A window never claims a Project B edit; precedence test; live-only restart test.
- [ ] `LOG_TAIL_TRAILING_WINDOW` is one public constant documented in `protocol.md`; optionally add **Attribution Window** and **Manual Work** to `GLOSSARY.md`.
