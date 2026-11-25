# 02: Stale-turn timeout for open Attribution Windows

**What to build:** An open Attribution Window stops covering file mutations once its session has gone quiet for `STALE_TURN_TIMEOUT` (10 minutes). This applies to both kinds of open window: a log-tailed turn window, and a hook window (an open tool call inside an open turn, as left behind by a `kill -9`ed Harness). A crashed or unterminated turn then stops turning the user's hand edits into Agent Compute. The rule is shared attribution logic that 1d's OpenCode and Codex Adapters get for free. See spec "Stale-turn timeout (attribution)".

**Blocked by:** None (can start immediately).

**Status:** ready-for-agent

- [ ] `STALE_TURN_TIMEOUT` is one public, documented constant next to `LOG_TAIL_HOLD`, `LOG_TAIL_TRAILING_WINDOW` and `WINDOW_CLOSE_GRACE`, with a test-only override in the watcher settings.
- [ ] Rule: an open window covers a mutation at `t` only if the session's latest event timestamp at decision time is ≥ `t − STALE_TURN_TIMEOUT`. Any event of any type or source counts as activity. The rule is checked lazily when a held mutation is decided, with no timer thread.
- [ ] Closed windows are unchanged (closing event + `WINDOW_CLOSE_GRACE`).
- [ ] Seam A tests use the existing `/v1/event` route and real watcher mutations:
  - a log-tailed turn with no events for the overridden timeout leaves a later mutation as Manual Work;
  - an open hook window (`TOOL_PRE` with no `TOOL_POST`) does the same;
  - a turn or tool call kept alive by events still claims the mutation;
  - results don't depend on arrival order within `LOG_TAIL_HOLD`.
- [ ] Docs are updated in this ticket: `docs/protocol.md` ("`FILE_MUTATION` attribution windows"), `GLOSSARY.md` ("Attribution Window"), the roadmap's 1c row, and the `docs/steps.md` stale-turn bullet (it now covers hook windows too).
