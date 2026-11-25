# 09: Gate tests green on all 3 OSes in CI

**What to build:** Push the integration branch to `origin` (rubixnet/bounty), confirm the 3-OS CI run is green (especially Windows `core.hooksPath=NUL` and the fsmonitor/bare-repo red-team tests), fixing any OS-specific failures; then tick the three 1b-owned Step 1 Exit Gate items in the spec.

**Blocked by:** 01, 02, 03, 04, 05, 06, 07, 08

**Status:** resolved

- [x] `cargo fmt`, `clippy -D warnings`, `cargo test --workspace` green on ubuntu, macos, windows in CI.
- [x] Spec gate checkboxes ticked with a link to the green run.

## Comments

- 3-OS CI green on `21deeb9`: https://github.com/rubixnet/bounty/actions/runs/45612520436. Windows ran the `core.hooksPath=NUL` argv test, the fsmonitor/hooks red-team tests (wrapper + daemon), and the planted bare-repo test; none ignored.
