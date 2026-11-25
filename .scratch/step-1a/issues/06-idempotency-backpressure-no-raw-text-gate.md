# 06: Duplicate handling, backpressure and the no-raw-text test

**What to build:** Adapter retries and fallback tailers can never double-count Logged Hours, a flood of events can't block a Harness, and there is proof that raw content never reaches disk. This ticket runs on top of the final ticket 05 check order, so the no-raw-text test exercises the real pipeline. It owns the 1a part of the Step 1 Exit Gate item "zero raw prompt text persisted in SQLite". See the single-writer decision and the Seam 2 testing decisions in `.scratch/step-1a/spec.md`.

**Blocked by:** 05

**Status:** resolved

- [x] `event_id` is the primary key, and inserts are idempotent. Sending the same event twice returns `202` both times (never `500` or another error), and exactly one row is stored.
- [x] The writer commits a batch when it reaches a size threshold, as well as on the 5-second timer.
- [x] When the writer channel is full, the bus returns `503` immediately instead of blocking the caller. A test can trigger this case.
- [x] Events accepted just before a graceful shutdown are present after it.
- [x] Sentinel test:
  - events that try to carry a sentinel string through unknown top-level or `metrics_only` fields return `400`;
  - valid events are also sent, then the daemon is shut down;
  - a scan of every table and column finds zero copies of the sentinel;
  - so does a scan of the raw database file bytes and any WAL file bytes.

## Comments

- **Scope extra: writer retry.** The ticket only asked that accepted events survive shutdown. The writer also keeps a batch whose commit fails (SQLite busy past its timeout, disk full, I/O error) and retries it one `commit_interval` later instead of dropping it. Inserts ignore a repeated `event_id`, so a retry never double-stores. While a failed batch is full the writer takes no more rows, so the channel fills and the bus answers `503`, which keeps memory bounded by `max_batch + queue_capacity`. Only the final commit at shutdown gives up, and it logs that the batch was dropped. Why: without it, one transient `SQLITE_BUSY` would silently lose up to a full batch of accepted events. Covered by `a_batch_whose_commit_fails_is_retried_not_dropped` in `tests/persistence.rs`.
