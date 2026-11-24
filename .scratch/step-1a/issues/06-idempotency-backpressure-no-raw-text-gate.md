# 06: Duplicate handling, backpressure and the no-raw-text test

**What to build:** Adapter retries and fallback tailers can never double-count Logged Hours, a flood of events can't block a Harness, and there is proof that raw content never reaches disk. This ticket runs on top of the final ticket 05 check order, so the no-raw-text test exercises the real pipeline. It owns the 1a part of the Step 1 Exit Gate item "zero raw prompt text persisted in SQLite". See the single-writer decision and the Seam 2 testing decisions in `.scratch/step-1a/spec.md`.

**Blocked by:** 05

**Status:** ready-for-agent

- [ ] `event_id` is the primary key, and inserts are idempotent. Sending the same event twice returns `202` both times (never `500` or another error), and exactly one row is stored.
- [ ] The writer commits a batch when it reaches a size threshold, as well as on the 5-second timer.
- [ ] When the writer channel is full, the bus returns `503` immediately instead of blocking the caller. A test can trigger this case.
- [ ] Events accepted just before a graceful shutdown are present after it.
- [ ] Sentinel test:
  - events that try to carry a sentinel string through unknown top-level or `metrics_only` fields return `400`;
  - valid events are also sent, then the daemon is shut down;
  - a scan of every table and column finds zero copies of the sentinel;
  - so does a scan of the raw database file bytes and any WAL file bytes.
