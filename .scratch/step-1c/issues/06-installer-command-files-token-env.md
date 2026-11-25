# 06: Installer, part 2: command files and `TRACTION_TOKEN` shell setup

**What to build:** The installer also writes Traction's `/stop-track` and `/resume-track` custom commands, which the model can never invoke, and makes `TRACTION_TOKEN` available in every new terminal by reading the token file at shell start. The token value is never copied into any config file. Windows also gets a per-user environment variable. Running the installer and opening a new shell is all the setup a user needs. See spec "Installer" (command files, token env, summary).

**Blocked by:** 04 (shared command body text), 05 (installer subcommand)

**Status:** ready-for-agent

- [ ] Writes `<claude config dir>/commands/stop-track.md` and `resume-track.md`. Each has frontmatter with a `description` and `disable-model-invocation: true`, a one-line body from 04's shared definition with no instructions to the model, and a Traction marker. A file is overwritten only when it is missing or marked. A user-written file with the same name is left untouched, and the installer warns that `/stop-track` will rely on the literal-text match only.
- [ ] `--profile <path>` (repeatable) defaults to the user's existing zsh, bash, fish and PowerShell profiles. Each gets a block between `# >>> traction >>>` and `# <<< traction <<<` that exports `TRACTION_TOKEN` from `~/.traction/daemon.token` at shell start, in each shell's own syntax. Rerunning replaces the block rather than adding another.
- [ ] On Windows, the installer sets `TRACTION_TOKEN` under `HKCU\Environment` without admin rights.
- [ ] The summary lists the settings path, the hooks installed, any version warning and the profiles touched, and ends with "open a new terminal (and restart Claude Code) so hooks can authenticate".
- [ ] Seam B tests cover: both command files written as specified and rewritten on rerun, a user-written file left untouched with a warning, the profile block added once and replaced on rerun, the block reading the token file and never containing the token, and nothing written outside the named files.
