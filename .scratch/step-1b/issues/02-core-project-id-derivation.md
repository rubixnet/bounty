# 02: Core Project ID derivation and origin normalization

**What to build:** Pure functions in `traction-core` that derive `git:` IDs (SHA-256 over root commit hex + normalized origin, fixed separator) and `dir:` IDs (SHA-256 over device secret + canonical root path, fixed separator), plus origin URL normalization. No file system or Git work in core. See spec "`traction-core` schema changes" (derivation + normalization bullets) and "Seam 1 → Project ID derivation".

**Blocked by:** None (can start immediately)

**Status:** ready-for-agent

- [ ] `sha2` added to core; derivation functions public and documented.
- [ ] Golden IDs pinned for fixed inputs (root commit, origin, secret, path).
- [ ] Origin normalization goldens: `https://github.com/A/B.git`, `git@github.com:A/B`, `ssh://git@github.com:22/A/B/` give the same ID; host lowercased; userinfo/port/scheme/trailing `.git` or `/` stripped.
- [ ] Different secrets or paths give different `dir:` IDs; empty origin handled.
- [ ] Formulas recorded in `docs/protocol.md` §2a (coordinate with 01 if it already landed).
