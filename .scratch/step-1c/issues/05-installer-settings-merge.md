# 05: Installer, part 1: `traction-daemon install claude-code` merges hooks into `settings.json`

**What to build:** One command wires Claude Code into Traction. It merges Traction's HTTP hooks into the user's `settings.json` (`timeout: 1`, `onFailure: "continue"`, token sent as `$TRACTION_TOKEN` through `allowedEnvVars`, never the raw value) and leaves everything else in the file untouched. It is idempotent and safe on upgrade, and it can never block Claude Code when the daemon is down. See spec "Installer".

**Blocked by:** 03 (uses the shared hook URL and hook-set definitions)

**Status:** ready-for-agent

- [ ] The binary gains subcommand dispatch. `install claude-code` takes `--settings`, `--claude-program` and `--port` (the `--profile` option arrives in 06). The Claude config dir is `$CLAUDE_CONFIG_DIR`, else `~/.claude`.
- [ ] Each hook in the set gets one Traction entry, in the shape the spec gives, inside a matcher group that matches all tools for the tool hooks.
- [ ] Ownership: an entry is Traction's when its `url` is the Traction hook URL on any port. A merge removes all Traction entries and then adds the current set. Foreign hooks and keys are preserved (key order kept as far as possible).
- [ ] A malformed `settings.json`, or a `hooks` value that isn't an object, gives a non-zero exit and leaves the file untouched. A missing file is created. The original is copied once to `settings.json.traction-backup` before the first edit and never overwritten. Writes go through temp file + rename in the same folder.
- [ ] `WorktreeCreate` is never written, and a Traction-owned `WorktreeCreate` entry is removed if found.
- [ ] Version check: `<claude-program> --version` runs with a 5 s kill timeout. Below `2.1.295` it warns and still installs. Unparseable output or a missing program gives a "could not check Claude Code version" warning and still installs.
- [ ] Seam B tests (binary, temp files, fake `claude` printing chosen versions) cover: merge preservation, idempotence (byte-identical second run), upgrade replacing old entries, malformed refusal, backup written once, missing file created, all four version cases, and no token value anywhere in the file.
- [ ] Every generated entry has `timeout` ≤ 1, `onFailure: "continue"` (never `"block"`), the `$TRACTION_TOKEN` header, and `TRACTION_TOKEN` in `allowedEnvVars`.
- [ ] **Fail-open firing test:** each generated entry's request (URL with the port swapped for a known-closed one, its headers, its timeout) fails within `timeout` + a small margin, on all 3 OSes.
