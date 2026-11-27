# Postgres and MinIO tests run on Linux only

Every Step 4 test that needs a real database or object store (Postgres 17 for `traction-server`, MinIO for the R2 `BlobStore`) runs on `ubuntu-latest` against service containers. They are not run on the Windows and macOS runners. `traction-server` deploys only on Linux (one binary, `systemd`, Caddy), and installing Postgres on the other runners is slow and flaky, which would make the 3-OS matrix unreliable without testing anything we ship there. We considered running Postgres on all three OSes and rejected it for that reason. We also rejected mocking the database: the real mechanics (partitioning, `ON CONFLICT`, advisory locks) are the point of these tests, and a stand-in would test the stand-in.

Everything else keeps the full 3-OS matrix: `cargo fmt`, `cargo clippy --workspace --all-targets`, every `traction-core` test, every `traction-daemon` test (including the sync client against a local test HTTP server built from `traction-core::sync`, and `server.token` permissions), and every `traction-server` test that needs no database (validation, token hashing, the CSRF check, refusing an inline secret).

## Consequences

- The Postgres tests read `TRACTION_TEST_DATABASE_URL`. Without it they print `SKIPPED` and pass, so a developer machine without Postgres can still run `cargo test --workspace`. The Linux CI job sets the variable and `TRACTION_REQUIRE_DB=1`, which turns a missing database into a failure, so CI can never skip them silently.
- "Passes real automated tests on all 3 OSes in CI" for Step 4's Exit Gate means this split (recorded in `docs/steps.md`).
- Nothing in `traction-server` may depend on a Linux-only API in code that the 3-OS jobs compile (it must still build and lint on Windows and macOS).
- The object store in CI is versitygw (POSIX backend, pinned by tag and digest), not MinIO: MinIO stopped publishing container images, and both `minio/minio` on Docker Hub and `quay.io/minio/minio` now refuse anonymous pulls. The `TRACTION_TEST_MINIO_*` variable names are kept. Any S3-compatible store that answers a presigned PUT with the wrong `Content-Length`, an expired URL or a tampered `X-Amz-Expires` with 403 passes `tests/minio_blob.rs`.
