# 02: `TractionEvent` type and validation (traction-core)

**What to build:** Adapter authors get one shared Traction Event type that matches `docs/protocol.md` §2 exactly, and the type has nowhere to put raw text. Malformed or content-carrying events are rejected when deserialized or validated, so every later consumer (the daemon's ingestion bus now, the server in Step 4) has one gate to rely on. See the `traction-core` decisions in `.scratch/step-1a/spec.md`.

**Blocked by:** 01

**Status:** resolved

- [x] `TractionEvent` has every protocol field: `schema_version` (currently 1), `event_id`, `timestamp_ms`, `harness_id`, `session_id`, `parent_orchestrator_id` (optional), `event_type`, `end_reason` (optional), `cwd`, `project_id`, `model_id` (optional), `network_class`, `metrics_only`.
- [x] `harness_id`, `event_type`, `network_class` and `end_reason` are closed enums that use the exact protocol wire spellings.
- [x] `metrics_only` is a typed struct whose fields are all optional: `prompt_char_len`, `repo_symbol_hits`, `has_stacktrace`, `tool_name`, `exit_code`, `diff_lines_delta`.
- [x] Unknown fields are rejected both at the top level (e.g. `"prompt": "SENTINEL"`) and inside `metrics_only` (e.g. `"stdout"`).
- [x] A single public validation step enforces:
  - `end_reason` is required on `TURN_END` and rejected on every other event type;
  - `tool_name` and `model_id` stay within a short length and the safe identifier character set;
  - `session_id` is bounded;
  - `cwd` is absolute and bounded;
  - `project_id` matches `git:<64 hex>` or `dir:<64 hex>`;
  - `event_id` is a valid UUID;
  - numbers are integers only (no floats).
- [x] Tests use only the crate's public API:
  - every `event_type` and `harness_id` round-trips through serialize and deserialize;
  - each rejection case above has its own failing test.
- [x] Code and tests use glossary terms: Traction Event, Harness, Adapter, Project.
