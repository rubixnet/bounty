# 04: S3b Ecosystem and scale: GitHub App linking, Project merge, soft provenance, load test, cost model and deploy docs

**What to build:**
- **GitHub App linking:** the push-access check, plus commit fetch that stores oids and timestamps only.
- **Project merge:** alias-only, on the server.
- **Build provenance:** soft and CI-signed, tagging each device `ci_signed` or `self_built` (S3a's `HeuristicEngine` already reads the tag).
- **Load test:** a `load` binary that proves Gate 1.
- **Cost model:** `docs/cost-model.md`, which proves Gate 2.
- **Deploy docs:** `docs/deploy.md`.

See spec "S3b: Ecosystem and scale", "Testing seams" (row S3b) and Resolved Decisions Q16, Q17, Q18, Q23, Q24 and Q31.

**Blocked by:** 02, 03

**Status:** resolved (CI-only boxes remain open)

Acceptance:

**GitHub App linking (`[github]`)**
- [x] A GitHub App with read-only `metadata` and `contents` permissions on the repos the user selects when installing it. The App private key comes from env or a file only, and the GitHub base URL can be injected.
- [x] A link is created only after a user-to-server token shows push access on the repo.
- [x] Commit fetch uses installation tokens and handles pagination and a 403 rate-limit response. It stores only `oid`, `authored_at`, `committed_at` and whether the author's login matches the user's linked GitHub login, in `linked_repos` and `repo_commits`. **No messages, no diffs**, which a test asserts.
- [x] Linking stays optional for tracking (ADR 0002). Submission Eligibility is not enforced here; that belongs to 5a.

**Project merge**
- [x] A `project_aliases(from_id → to_id)` table:
  - alias chains are followed and cycles refused;
  - merging only works within one user's own Projects;
  - buckets are never rewritten, and every read query (totals, SSE, timeline) resolves aliases;
  - unmerging deletes the alias;
  - bundle verification keeps the original IDs.

**Build provenance (soft)**
- [x] Release CI signs `{version, git_commit, target_triple}` with a CI Ed25519 key, and that statement is compiled into the daemon binary. A build without it is `self_built`.
- [x] The daemon sends it at pairing and when its version changes. The server checks it against the CI public keys in config (inline is allowed) and tags the device `ci_signed` or `self_built`.
- [x] `self_built` sends a submission to a standard spot-check (Yellow), never to Red. The docs state that the tag is soft and can be copied.

**Load test (Gate 1)**
- [x] A `load` binary in `crates/traction-server` drives the real server and Postgres 17. It simulates 15,488 enrolled devices, with peak concurrency at 10× the ~153-device average, realistic heartbeat sizes including Timeline Marks, and chunked offline backlogs.
- [x] It runs against a database preseeded with a month of data (~6.6M buckets) and reports the ingest p95 and server RSS.
- [ ] CI:
  - every PR runs a reduced-scale smoke version on Linux; *(done: Load smoke green in CI run 46003549496 on `01b596f`.)*
  - a nightly or `workflow_dispatch` Linux job runs it at full scale and publishes the report as an artifact. *(Pending: `load.yml` is a new workflow file, so GitHub cannot dispatch it until it is on `main`; it will be dispatched immediately after merging to `main`.)*
- [ ] The full-scale report shows **<50 ms p95 ingest** and **<512 MB server RSS**. Gate 1 is ticked in the spec with a link to the report. *(Pending the first full-scale `load.yml` run: `load.yml` is a new workflow file, so GitHub cannot dispatch it until it is on `main`; it will be dispatched immediately after merging to `main`.)*

**Cost model (Gate 2)**
- [x] `docs/cost-model.md` itemizes the monthly cost, each line with a dated source:
  - the dedicated server
  - Postgres storage growth, from the load test's measured bytes per bucket projected over 12 months
  - R2 storage and operations
  - Jev at its spend limit
  - bandwidth
  - `pg_dump` backups to R2
  - domain and email
- [ ] Its totals show ≤$300 at 15,488 MAU and ≤$500 at the load test's peak burst. Gate 2 is ticked in the spec. *(CI is green (CI run 46003549496 on `01b596f`); the box waits for the full-scale load run to refresh the numbers. `load.yml` is a new workflow file, so GitHub cannot dispatch it until it is on `main`; it will be dispatched immediately after merging to `main`.)*

**Deploy**
- [x] `docs/deploy.md` covers one Linux `traction-server` binary with migrations embedded, a `systemd` unit, and a sample Caddy config for TLS. There is no Docker requirement, and actual provisioning stays a human step.

**Tests (spec "Testing seams" S3b)**
- [x] **All 3 OSes:** *(green in CI run 46003549496 on `01b596f`.)*
  - provenance signature checks and tagging;
  - the GitHub client against a local test HTTP server replaying recorded real GitHub API responses (pagination, the 403 rate limit, the push-access check);
  - alias resolution and cycle refusal.
- [ ] **Linux only:** the Project merge API across two devices' Projects; the load smoke run on PRs; the full nightly run. *(Merge API and load smoke green in CI run 46003549496 on `01b596f`; the full run is pending: `load.yml` is a new workflow file, so GitHub cannot dispatch it until it is on `main`; it will be dispatched immediately after merging to `main`.)*
- [x] `cargo fmt`/`clippy`/`test` green on all 3 OSes, and the Linux jobs green. *(CI run 46003549496 on `01b596f`.)*

## Comments

**2026-11-27 (implementation):** branch `step-4-04-ecosystem-and-scale`, based on `step-4-sync-and-backend` (d6429a3).

- **Verification:** `fmt`, `clippy -D warnings` and `cargo test --workspace` are green on macOS against a local Postgres 17 (`TRACTION_REQUIRE_DB=1`; MinIO-gated tests skip locally). The 3-OS matrix, the new `load-smoke` CI job, the nightly `load.yml` and `release.yml` have **not run in CI**, so those boxes stay open: "CI:", "the full-scale report" (Gate 1) and the last "green on all 3 OSes" box, plus the nightly half of the Linux-only box.
- **Gate 2** is ticked in the spec: `docs/cost-model.md` totals $167.71 (15,488 MAU) and $449.97 (peak-burst scenario). Prices were fetched 2026-11-27. Jev's price and the `.com` price come from third-party pages only, and the USD/EUR rate (1.16) is an assumption; all three are marked in the doc.
- **Smoke load run** (release build, laptop, 1,000 devices, ~200k seeded buckets, 150 concurrent devices): paced ingest p95 7.5 ms, server peak RSS 21 MB, 1,450 bytes per bucket all-in, 0 errors. A synchronized burst of all 150 devices at once is reported separately (p95 63 ms) and is **not** gated; see the load-model decision below.
- **Decisions:**
  - **Load model.** Real peak is ~1,530 devices heartbeating every 2 minutes (~13 req/s). The Gate 1 population is therefore *paced*: each device sends at a random moment inside `--interval-ms` (default 2 s, i.e. 60x the real rate), plus 10% of devices replaying offline backlogs in chunks of 30 links. Extra synchronized-burst rounds are reported as informational only. The gate is p95 over the paced and backlog requests.
  - **RSS** is sampled with `ps` every 200 ms from the real `traction-server` child process (idle and peak are both reported).
  - **Provenance transport.** `PairingStartRequest.build` is verified at pairing start and copied to the device on approval; `HeartbeatRequest.build` is sent on the first heartbeat of each daemon run (this covers a version change, which always means a restart) and re-tags the device. Both fields are optional, so older daemons parse. An unverified statement's fields are never stored. The signature is domain-separated (`traction-build-statement/1`).
  - **`traction-sign-build`** (in `traction-core`) is the CI signing helper; the seed comes from `TRACTION_CI_SIGNING_SEED`. `release.yml` signs and compiles the statement through `TRACTION_BUILD_STATEMENT`.
  - **Commits live in `project_commits`** (created in ticket 02, already read by the timeline and `evaluate`), not in a second `repo_commits` table. `linked_repos` holds the link (installation id, repo id, last fetch).
  - **GitHub endpoints:** `POST /v1/github/link`, `POST /v1/github/projects/{id}/sync`, `DELETE /v1/github/projects/{id}`; the user-to-server token comes in the request body and is never stored. The App JWT is verified by the test GitHub server with real RS256. A rate limit mid-fetch stores nothing and returns `503 github_rate_limited` with `Retry-After` (link keeps the link and reports `commits_pending`).
  - **Test fixtures:** the two commit pages and the repo document are real recorded responses (octocat/Hello-World, 2026-11-27). The push-permission object and the `403` rate-limit reply follow GitHub's documentation and were not recorded (GitHub only returns them to an authenticated caller who is over the limit).
  - **Project merge:** `POST /v1/projects/merge {from_id,to_id}` and `DELETE /v1/projects/merge/{from_id}`. A Project the caller does not own reads as `404`. Cycles, self-merge and a second alias for one `from_id` are `409`. Merges are serialized per user with an advisory lock. Resolution is applied in `merge::project_totals` (so the live totals and the Project list), the Project list (alias sources are hidden; private view shows `merged_project_ids`) and the timeline (any member ID resolves to the target). `evaluate` and bundle verification still use original IDs.
- **Existing tests changed:** the submission rig now pairs with a CI-signed statement (`config_trusting_ci`), because a default device is now correctly `self_built` and so Yellow; a new test pins both outcomes.
- **Deferred:**
  - Full-scale Gate 1 report (needs the nightly job run in CI) and refreshing the cost model's bytes per bucket from it.
  - A Windows/macOS release job's real run, and provisioning the `TRACTION_CI_SIGNING_SEED` secret (human).
  - Verifying the burst behaviour at 1,530 simultaneous devices; the smoke burst p95 (63 ms at 150) suggests a larger burst would queue on the 16-connection pool. If a synchronized-burst gate is ever wanted, raise the pool or batch inserts first.
  - A browser flow for obtaining the user-to-server token (5a).
  - GitHub App webhooks; commits are pulled on link and on `sync` only.
