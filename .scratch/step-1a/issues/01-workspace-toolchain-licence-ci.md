# 01: Workspace, toolchain, licence and CI workflow file

**What to build:** A contributor can clone the repo and run fmt, clippy and tests with one pinned Rust toolchain on any OS. The repo contains the AGPLv3 licence, a Cargo workspace with empty `traction-core` and `traction-daemon` crates (no `traction-server`, `packages/` or `apps/`), and a 3-OS GitHub Actions workflow that is written but not run yet; ticket 09 runs it later. See the "Toolchain and workspace" and "CI and repo" decisions in `.scratch/step-1a/spec.md`.

**Blocked by:** None (can start immediately)

**Status:** ready-for-agent

- [ ] Rust is installed with `rustup`. `rust-toolchain.toml` pins an exact stable version (not the `stable` channel name) with the `rustfmt` and `clippy` components.
- [ ] The repo root has the full AGPLv3 `LICENSE` text, and both crates declare `license = "AGPL-3.0-only"`.
- [ ] The workspace lists only `traction-core` and `traction-daemon`, and shared dependency versions are declared once at the workspace level.
- [ ] `traction-core` has no OS, I/O or async dependencies.
- [ ] The CI workflow runs on push and pull request with a matrix of `ubuntu-latest`, `macos-latest` and `windows-latest`. It runs `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace`.
- [ ] All three commands pass locally.
- [ ] No GitHub repo is created and nothing is pushed.
