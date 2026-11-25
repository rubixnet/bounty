# 05: Installer, part 1: `traction-daemon install claude-code` merges hooks into `settings.json`

**What to build:** One command wires Claude Code into Traction. It merges Traction's HTTP hooks into the user's `settings.json` (`timeout: 1`, `onFailure: "continue"`, token sent as `$TRACTION_TOKEN` through `allowedEnvVars`, never the raw value) and leaves everything else in the file untouched. It is idempotent and safe on upgrade, and it can never block Claude Code when the daemon is down. See spec "Installer".

**Blocked by:** 03 (uses the shared hook URL and hook-set definitions)

**Status:** resolved

- [x] The binary gains subcommand dispatch. `install claude-code` takes `--settings`, `--claude-program` and `--port` (the `--profile` option arrives in 06). The Claude config dir is `$CLAUDE_CONFIG_DIR`, else `~/.claude`.
- [x] Each hook in the set gets one Traction entry, in the shape the spec gives, inside a matcher group that matches all tools for the tool hooks.
- [x] Ownership: an entry is Traction's when its `url` is the Traction hook URL on any port. A merge removes all Traction entries and then adds the current set. Foreign hooks and keys are preserved (key order kept as far as possible).
- [x] A malformed `settings.json`, or a `hooks` value that isn't an object, gives a non-zero exit and leaves the file untouched. A missing file is created. The original is copied once to `settings.json.traction-backup` before the first edit and never overwritten. Writes go through temp file + rename in the same folder.
- [x] `WorktreeCreate` is never written, and a Traction-owned `WorktreeCreate` entry is removed if found.
- [x] Version check: `<claude-program> --version` runs with a 5 s kill timeout. Below `2.1.295` it warns and still installs. Unparseable output or a missing program gives a "could not check Claude Code version" warning and still installs.
- [x] Seam B tests (binary, temp files, fake `claude` printing chosen versions) cover: merge preservation, idempotence (byte-identical second run), upgrade replacing old entries, malformed refusal, backup written once, missing file created, all four version cases, and no token value anywhere in the file.
- [x] Every generated entry has `timeout` ≤ 1, `onFailure: "continue"` (never `"block"`), the `$TRACTION_TOKEN` header, and `TRACTION_TOKEN` in `allowedEnvVars`.
- [x] **Fail-open firing test:** each generated entry's request (URL with the port swapped for a known-closed one, its headers, its timeout) fails within `timeout` + a small margin, on all 3 OSes.

## Comments

- Installer lives in `crates/traction-daemon/src/install/`: `claude_code.rs` (`ClaudeCodeInstall` options with `for_user()` defaults and `run()`, `InstallReport` whose `Display` is the summary, `InstallError`), `settings.rs` (the merge), `version.rs` (bounded `--version` check, reusing `git::wait_bounded`), `ordered_json.rs` (keeps the user's key order; the workspace's `serde_json::Map` must stay sorted for canonical JSON, so `preserve_order` is not an option). `main.rs` dispatches `install claude-code` and parses its flags.
- Claude config dir rule: `crates/traction-daemon/src/claude_code/config_dir.rs` (`claude_config_dir()`, shared with 07's tailer).
- Extension point for 06: add `profiles` (`--profile`, parsed in `main.rs::install`) and the commands folder to `ClaudeCodeInstall`, add the command-file and token-env steps to `ClaudeCodeInstall::run` after the settings merge, and add profiles touched plus the closing "open a new terminal" line to `InstallReport` and its `Display`. Warnings already flow through `InstallReport::warnings` (printed to stderr by `main.rs`).
- Tests: `crates/traction-daemon/tests/install_claude_code.rs` (Seam B, fake `claude` as a `sh` script or `.cmd`).
