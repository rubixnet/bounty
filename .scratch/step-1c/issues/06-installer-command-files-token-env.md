# 06: Installer, part 2: command files and `TRACTION_TOKEN` shell setup

**What to build:** The installer also writes Traction's `/stop-track` and `/resume-track` custom commands, which the model can never invoke, and makes `TRACTION_TOKEN` available in every new terminal by reading the token file at shell start. The token value is never copied into any config file. Windows also gets a per-user environment variable. Running the installer and opening a new shell is all the setup a user needs. See spec "Installer" (command files, token env, summary).

**Blocked by:** 04 (shared command body text), 05 (installer subcommand)

**Status:** resolved

- [x] Writes `<claude config dir>/commands/stop-track.md` and `resume-track.md`. Each has frontmatter with a `description` and `disable-model-invocation: true`, a one-line body from 04's shared definition with no instructions to the model, and a Traction marker. A file is overwritten only when it is missing or marked. A user-written file with the same name is left untouched, and the installer warns that `/stop-track` will rely on the literal-text match only.
- [x] `--profile <path>` (repeatable) defaults to the user's existing zsh, bash, fish and PowerShell profiles. Each gets a block between `# >>> traction >>>` and `# <<< traction <<<` that exports `TRACTION_TOKEN` from `~/.traction/daemon.token` at shell start, in each shell's own syntax. Rerunning replaces the block rather than adding another.
- [x] On Windows, the installer sets `TRACTION_TOKEN` under `HKCU\Environment` without admin rights.
- [x] The summary lists the settings path, the hooks installed, any version warning and the profiles touched, and ends with "open a new terminal (and restart Claude Code) so hooks can authenticate".
- [x] Seam B tests cover: both command files written as specified and rewritten on rerun, a user-written file left untouched with a warning, the profile block added once and replaced on rerun, the block reading the token file and never containing the token, and nothing written outside the named files.

## Comments

- Code: `crates/traction-daemon/src/install/commands.rs` (command files; marker `COMMAND_MARKER` is a frontmatter comment, body is exactly `Command::body`), `profile.rs` (marked block per syntax: `*.ps1` PowerShell, `*.fish` fish, else POSIX; default profile discovery), `user_env.rs` (Windows `RegSetKeyValueW` under `HKCU\Environment` + `WM_SETTINGCHANGE` broadcast), `files.rs` (temp + rename, follows symlinks, keeps permissions). Steps are wired in `ClaudeCodeInstall::run`; `--profile` parsing in `main.rs`.
- The commands folder is `commands/` next to `settings.json` (by default `<claude config dir>/commands`), so `--settings` keeps tests hermetic without another flag.
- Any `--profile` replaces the default list. A profile that cannot be written (or has an unclosed block) is a warning, not a failure; no profile found is a summary line, not a warning.
- Windows: the registry value must be the token itself (the registry cannot read a file), so the installer creates the token file if the daemon never ran. `--windows-env-key <subkey>` redirects the write; tests use a scratch `HKCU\Software\TractionTest\<tmp>` key and delete it, so they never touch `HKCU\Environment`.
- Tests: Seam B in `tests/install_claude_code.rs` (incl. sourcing the POSIX block with `sh`, and the PowerShell block with `pwsh`/`powershell` where present); Seam A `the_literal_command_and_the_installed_files_body_are_both_blocked` in `tests/claude_code_pause.rs`. Windows/Linux were type-checked locally with clippy cross targets (fake C compiler for bundled SQLite); runtime coverage there is CI.
