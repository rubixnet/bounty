@AGENTS.md

## Claude Code Notes
- Before starting Step 4 (Jev integration), ensure the TypeSafe skill is installed (`typesafe@typesafe-ai`) and consult its live `v1/systemone` contract rather than guessing fields.
- Keep commits small, scoped to one ticket/sub-step, and verified against the current Step's Exit Gate in `docs/steps.md`.- Sub-step plan lives in `docs/roadmap.md`. Work each Step as small sub-steps (e.g. Step 1a, 1b): `/grill-with-docs` → `/to-spec` → `/to-tickets` → `/implement` per ticket, clearing context between tickets so no session exceeds ~150k tokens.

## Agent skills

### Issue tracker

Local markdown under `.scratch/<step-slug>/` (spec + one file per ticket). See `docs/agents/issue-tracker.md`.

### Triage labels

Default five-role vocabulary (`needs-triage`, `needs-info`, `ready-for-agent`, `ready-for-human`, `wontfix`). See `docs/agents/triage-labels.md`.

### Domain docs

Single-context: root `GLOSSARY.md` + `docs/adr/`. See `docs/agents/domain.md`.
