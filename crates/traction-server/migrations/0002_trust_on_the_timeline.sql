-- Step 4, slice S2: anchor timing flags, clock skew, Project links, unsigned
-- Timeline Marks and commit oids. Partitions are still created by the server.

-- `skew_ms = server_received_at - sent_at_ms` of the newest heartbeat, and
-- the flag while |skew| > MAX_CLOCK_SKEW.
ALTER TABLE devices
    ADD COLUMN skew_ms    BIGINT,
    ADD COLUMN clock_skew BOOLEAN NOT NULL DEFAULT FALSE;

-- Every heartbeat's skew is stored on its Anchor (written once, never
-- updated); `late_anchor` is the tip window's lateness.
ALTER TABLE anchors
    ADD COLUMN skew_ms     BIGINT  NOT NULL DEFAULT 0,
    ADD COLUMN late_anchor BOOLEAN NOT NULL DEFAULT FALSE;

-- Timing tags are set when a bucket is first anchored and never change.
-- `offline_buffered` rows count as Logged Hours only, never as timing proof.
ALTER TABLE work_buckets
    ADD COLUMN late_anchor      BOOLEAN NOT NULL DEFAULT FALSE,
    ADD COLUMN offline_buffered BOOLEAN NOT NULL DEFAULT FALSE;

-- Set from the website (S3b), never by the daemon.
ALTER TABLE projects ADD COLUMN linked_repo TEXT;

-- Unsigned, content-free marks (`docs/adr/0005`): counts and the harness set.
CREATE TABLE timeline_marks (
    user_id           TEXT   NOT NULL,
    device_id         TEXT   NOT NULL,
    project_id        TEXT   NOT NULL,
    minute_start_ms   BIGINT NOT NULL,
    mutations_manual  BIGINT NOT NULL,
    mutations_harness BIGINT NOT NULL,
    turns_started     BIGINT NOT NULL,
    tool_failures     BIGINT NOT NULL,
    harnesses         TEXT[] NOT NULL,
    PRIMARY KEY (device_id, project_id, minute_start_ms)
) PARTITION BY RANGE (minute_start_ms);
CREATE INDEX timeline_marks_user_project
    ON timeline_marks (user_id, project_id, minute_start_ms);

-- GitHub commit data is `oid`, `authored_at`, `committed_at` and an
-- author-match flag only (no messages, no diffs). Filled by S3b's client.
CREATE TABLE project_commits (
    user_id         TEXT    NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    project_id      TEXT    NOT NULL,
    oid             TEXT    NOT NULL,
    authored_at_ms  BIGINT  NOT NULL,
    committed_at_ms BIGINT  NOT NULL,
    author_match    BOOLEAN NOT NULL,
    PRIMARY KEY (user_id, project_id, oid)
);
CREATE INDEX project_commits_time
    ON project_commits (user_id, project_id, committed_at_ms);
