# Timeline Marks are unsigned, content-free context

A Timeline Mark is a per-Project, per-minute record of counts (file mutations by origin, turns started, tool failures) and the set of `harness_id`s seen. The daemon's finalizer computes it from stored events into its own `timeline_mark` table. It is synced beside the Work Buckets, stored in a partitioned `timeline_marks` table, and shown on the Audit Strip so a reviewer can line activity up with the buckets. It is **not** part of the hash chain, **not** signed, and every API response labels it `unsigned`.

We considered three options and chose this one:

- **(a) No marks.** The Audit Strip would show buckets and commits only. A reviewer could not see, for example, that a minute of "agent work" had no turn behind it. Rejected: too little context for the human review that Yellow depends on.
- **(b) Unsigned marks (chosen).** Cheap, and enough for context. The cost is that a tampered or invented mark is not detectable the way a tampered bucket is, so a mark can never carry weight on its own.
- **(c) Marks inside the chain.** Signing them means changing the `LinkPayload` contents, which bumps `bundle_schema` and freezes the mark format into every shipped bundle before the Audit Strip has shown which counts reviewers actually use. Deferred, not rejected: if reviewers come to rely on marks, signing them can be a later `bundle_schema` bump with its own migration; nothing in this decision blocks that.

## What a mark may contain

Counts and the Harness ID set only. The type is `deny_unknown_fields` and has no free-text slot: no `cwd`, `model_id`, session ID, path, file name or prompt can be expressed in it. The daemon computes marks in memory from events that were already stored (which hold none of that content in derived form) and a canary test proves nothing of the sort reaches the `timeline_mark` table or the heartbeat.

## Consequences

- **Marks never decide anything.** No deterministic failure and no `HeuristicEngine` or Jev rule may treat a mark as proof. They may inform a human reviewer.
- **A refused mark never blocks sync.** The server rejects a malformed mark with a value-free reason code like any item, but the daemon ignores rejected marks (it never holds its sync cursor back for one) because they are context, not evidence.
- **Marks are computed once.** A minute's mark is written once with the bucket batch and is never rewritten by late events, like attribution (live-only).
- **Deletion.** `DELETE /v1/me` removes a user's marks with everything else.
- **Signing later** means a new `bundle_schema`, a Chain Link payload that includes a marks digest, and a migration of the daemon table. The wire type does not change.
