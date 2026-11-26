# Verify the chain at submission, not on every heartbeat

The server stores each Anchor as `(device, key_id, seq, entry_hash, window_start_ms, server_received_at)` and runs full chain verification (`verify_chains`, `verify_bundle`) only when an Audit Bundle is submitted. A heartbeat is never verified incrementally. We considered verifying each heartbeat's links as they arrive and rejected it: the server would need every link's payload and signature on every request (the heartbeat carries only the tip and the rows, not the chain), the cost would scale with 15,488 devices every two minutes, and it would add nothing the submission check does not already catch. At submission the server holds the device's whole history of Anchors and compares them against the bundle's links.

An Anchor is never overwritten. `(device, key_id, seq)` is idempotent only when `entry_hash` matches; a different `entry_hash` for an already-anchored `seq` is `409 Conflict` and the stored Anchor does not change. Anchors are what make cutting off a chain's tail, or rewriting it, visible later: every stored Anchor must appear in the bundle with the same `entry_hash`.

`traction-core` stays unchanged: `verify_chain`, `verify_chains`, `verify_bundle` and the rollup types are reused as they are. Step 4 only adds to core (the `sync` wire types, the pure ingest `validate_*` functions, later `TimelineMark`). The server is a consumer of the verifier, never a second implementation of it.

## Consequences

- A live total (Logged Hours) is synced but not verified. Live pages must never present it as verified; only a verified Audit Bundle is proof.
- The deterministic failures (a chain verification error, an Anchor that matches no link, a row that differs from the live-synced bucket) are Red before any `DecisionEngine` runs.
- Heartbeat handling stays cheap: validation of each row, one insert for the Anchor and typed-column inserts for the rows.
