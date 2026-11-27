-- Step 4, slice S1: accounts, Pairing, devices, Anchors and synced rows.
-- Runtime-checked SQLx queries only; partitions are created by the server
-- (startup, daily and on demand), never here, and there is no DEFAULT
-- partition: a row outside every partition is a structured error.

CREATE TABLE users (
    id           TEXT PRIMARY KEY,
    sub          TEXT NOT NULL UNIQUE,
    display_name TEXT NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE devices (
    id         TEXT PRIMARY KEY,
    user_id    TEXT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    label      TEXT NOT NULL,
    -- SHA-256 of the 32-byte device token; the token itself is never stored.
    token_hash BYTEA NOT NULL UNIQUE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    revoked_at TIMESTAMPTZ
);
CREATE INDEX devices_user ON devices (user_id);

-- Every Device Signing Key ever pinned for a device. Revoking a device stops
-- new Anchors, but its keys stay here so history keeps verifying.
CREATE TABLE device_keys (
    device_id  TEXT NOT NULL REFERENCES devices (id) ON DELETE CASCADE,
    key_id     TEXT NOT NULL,
    public_key TEXT NOT NULL,
    pinned_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (device_id, key_id)
);

CREATE TABLE pairings (
    pair_code          TEXT PRIMARY KEY,
    poll_secret_hash   BYTEA NOT NULL,
    public_key         TEXT NOT NULL,
    previous_key_id    TEXT,
    device_label       TEXT NOT NULL,
    created_at_ms      BIGINT NOT NULL,
    expires_at_ms      BIGINT NOT NULL,
    approved_user_id   TEXT REFERENCES users (id) ON DELETE CASCADE,
    approved_device_id TEXT REFERENCES devices (id) ON DELETE CASCADE,
    token_delivered    BOOLEAN NOT NULL DEFAULT FALSE
);

CREATE TABLE projects (
    user_id            TEXT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    project_id         TEXT NOT NULL,
    project_name       TEXT NOT NULL,
    root_path          TEXT NOT NULL,
    home_relative_path TEXT,
    os_username        TEXT NOT NULL,
    updated_at_ms      BIGINT NOT NULL,
    PRIMARY KEY (user_id, project_id)
);

-- Typed columns that round-trip exactly to `WorkBucket` (no JSONB).
CREATE TABLE work_buckets (
    user_id                 TEXT NOT NULL,
    device_id               TEXT NOT NULL,
    project_id              TEXT NOT NULL,
    minute_start_ms         BIGINT NOT NULL,
    ruleset_id              TEXT NOT NULL,
    agent_compute_sec       BIGINT NOT NULL,
    idle_autonomous_sec     BIGINT NOT NULL,
    human_active_sec        BIGINT NOT NULL,
    permission_wait_ms      BIGINT NOT NULL,
    commits_human           BIGINT NOT NULL,
    commits_agent           BIGINT NOT NULL,
    injected_hid_detected   BOOLEAN NOT NULL,
    auto_approver_suspected BOOLEAN NOT NULL,
    telemetry_tier          TEXT NOT NULL
        CHECK (telemetry_tier IN ('full', 'reduced', 'idle_only', 'none')),
    tier_limited            BOOLEAN NOT NULL,
    axis_steering           BIGINT NOT NULL,
    axis_specificity        BIGINT NOT NULL,
    axis_friction           BIGINT NOT NULL,
    axis_comfort_delta      BIGINT NOT NULL,
    verified_struggle       BIGINT NOT NULL,
    struggle_credit_sec     BIGINT NOT NULL,
    PRIMARY KEY (device_id, project_id, minute_start_ms, ruleset_id)
) PARTITION BY RANGE (minute_start_ms);
CREATE INDEX work_buckets_user ON work_buckets (user_id, minute_start_ms);

-- Typed columns that round-trip exactly to `ActivityRollup`.
CREATE TABLE activity_rollups (
    user_id                    TEXT NOT NULL,
    device_id                  TEXT NOT NULL,
    window_start_ms            BIGINT NOT NULL,
    ruleset_id                 TEXT NOT NULL,
    minutes_observed           BIGINT NOT NULL,
    share_ide                  BIGINT NOT NULL,
    share_terminal             BIGINT NOT NULL,
    share_browser              BIGINT NOT NULL,
    share_ai_desktop           BIGINT NOT NULL,
    share_other                BIGINT NOT NULL,
    switch_rate_milli          BIGINT NOT NULL,
    mean_cadence_entropy_milli BIGINT,
    injected_ratio_milli       BIGINT,
    telemetry_tier             TEXT NOT NULL
        CHECK (telemetry_tier IN ('full', 'reduced', 'idle_only', 'none')),
    PRIMARY KEY (device_id, window_start_ms, ruleset_id)
) PARTITION BY RANGE (window_start_ms);

-- One Anchor per (device, key_id, seq). A partitioned table cannot enforce
-- that uniqueness across partitions, so ingest serializes per device under an
-- advisory lock and checks `anchors_device_key_seq` before inserting; a stored
-- Anchor is never updated.
CREATE TABLE anchors (
    device_id          TEXT NOT NULL,
    key_id             TEXT NOT NULL,
    seq                BIGINT NOT NULL,
    entry_hash         TEXT NOT NULL,
    window_start_ms    BIGINT NOT NULL,
    sync_mode          TEXT NOT NULL CHECK (sync_mode IN ('realtime', 'offline_buffered')),
    sent_at_ms         BIGINT NOT NULL,
    server_received_us BIGINT NOT NULL,
    server_received_at TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (device_id, key_id, seq, server_received_at)
) PARTITION BY RANGE (server_received_at);
CREATE INDEX anchors_device_key_seq ON anchors (device_id, key_id, seq);
CREATE INDEX anchors_device_received ON anchors (device_id, server_received_us DESC);
