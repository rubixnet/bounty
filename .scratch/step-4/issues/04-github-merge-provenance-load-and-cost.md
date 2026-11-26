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

**Status:** ready-for-agent

Acceptance:

**GitHub App linking (`[github]`)**
- [ ] A GitHub App with read-only `metadata` and `contents` permissions on the repos the user selects when installing it. The App private key comes from env or a file only, and the GitHub base URL can be injected.
- [ ] A link is created only after a user-to-server token shows push access on the repo.
- [ ] Commit fetch uses installation tokens and handles pagination and a 403 rate-limit response. It stores only `oid`, `authored_at`, `committed_at` and whether the author's login matches the user's linked GitHub login, in `linked_repos` and `repo_commits`. **No messages, no diffs**, which a test asserts.
- [ ] Linking stays optional for tracking (ADR 0002). Submission Eligibility is not enforced here; that belongs to 5a.

**Project merge**
- [ ] A `project_aliases(from_id → to_id)` table:
  - alias chains are followed and cycles refused;
  - merging only works within one user's own Projects;
  - buckets are never rewritten, and every read query (totals, SSE, timeline) resolves aliases;
  - unmerging deletes the alias;
  - bundle verification keeps the original IDs.

**Build provenance (soft)**
- [ ] Release CI signs `{version, git_commit, target_triple}` with a CI Ed25519 key, and that statement is compiled into the daemon binary. A build without it is `self_built`.
- [ ] The daemon sends it at pairing and when its version changes. The server checks it against the CI public keys in config (inline is allowed) and tags the device `ci_signed` or `self_built`.
- [ ] `self_built` sends a submission to a standard spot-check (Yellow), never to Red. The docs state that the tag is soft and can be copied.

**Load test (Gate 1)**
- [ ] A `load` binary in `crates/traction-server` drives the real server and Postgres 17. It simulates 15,488 enrolled devices, with peak concurrency at 10× the ~153-device average, realistic heartbeat sizes including Timeline Marks, and chunked offline backlogs.
- [ ] It runs against a database preseeded with a month of data (~6.6M buckets) and reports the ingest p95 and server RSS.
- [ ] CI:
  - every PR runs a reduced-scale smoke version on Linux;
  - a nightly or `workflow_dispatch` Linux job runs it at full scale and publishes the report as an artifact.
- [ ] The full-scale report shows **<50 ms p95 ingest** and **<512 MB server RSS**. Gate 1 is ticked in the spec with a link to the report.

**Cost model (Gate 2)**
- [ ] `docs/cost-model.md` itemizes the monthly cost, each line with a dated source:
  - the dedicated server
  - Postgres storage growth, from the load test's measured bytes per bucket projected over 12 months
  - R2 storage and operations
  - Jev at its spend limit
  - bandwidth
  - `pg_dump` backups to R2
  - domain and email
- [ ] Its totals show ≤$300 at 15,488 MAU and ≤$500 at the load test's peak burst. Gate 2 is ticked in the spec.

**Deploy**
- [ ] `docs/deploy.md` covers one Linux `traction-server` binary with migrations embedded, a `systemd` unit, and a sample Caddy config for TLS. There is no Docker requirement, and actual provisioning stays a human step.

**Tests (spec "Testing seams" S3b)**
- [ ] **All 3 OSes:**
  - provenance signature checks and tagging;
  - the GitHub client against a local test HTTP server replaying recorded real GitHub API responses (pagination, the 403 rate limit, the push-access check);
  - alias resolution and cycle refusal.
- [ ] **Linux only:** the Project merge API across two devices' Projects; the load smoke run on PRs; the full nightly run.
- [ ] `cargo fmt`/`clippy`/`test` green on all 3 OSes, and the Linux jobs green.

## Comments
