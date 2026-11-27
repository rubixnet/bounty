-- Step 4, slice S3a: Audit Bundle submissions and their triage decisions.

-- One row per `POST /v1/bundles`. The bundle bytes live in the blob store
-- under `blob_key`; they are deleted with the account (`DELETE /v1/me`).
-- `status`: `pending` (URL issued), `submitted` (verified, triaged and
-- stored), `rejected` (could not be read as a bundle at all).
CREATE TABLE bundles (
    id               TEXT PRIMARY KEY,
    user_id          TEXT   NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    device_id        TEXT   NOT NULL REFERENCES devices (id) ON DELETE CASCADE,
    project_id       TEXT   NOT NULL,
    blob_key         TEXT   NOT NULL UNIQUE,
    size_bytes       BIGINT NOT NULL CHECK (size_bytes > 0),
    status           TEXT   NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending', 'submitted', 'rejected')),
    created_at_ms    BIGINT NOT NULL,
    expires_at_ms    BIGINT NOT NULL,
    completed_at_ms  BIGINT
);
CREATE INDEX bundles_user ON bundles (user_id, created_at_ms);
CREATE INDEX bundles_device ON bundles (device_id);

-- The one decision of a submission. `engine_id` is the engine that actually
-- decided (`deterministic` for a verification failure, which no engine can
-- upgrade). The anonymized input is kept so a reviewer can see exactly what
-- the engine saw; it holds no user, path, username or Project name.
CREATE TABLE triage_decisions (
    bundle_id      TEXT PRIMARY KEY REFERENCES bundles (id) ON DELETE CASCADE,
    verdict        TEXT   NOT NULL CHECK (verdict IN ('green', 'yellow', 'red')),
    reasons        TEXT[] NOT NULL,
    engine_id      TEXT   NOT NULL,
    rubric_version TEXT   NOT NULL,
    -- The stable code of the deterministic failure, when there was one.
    failure        TEXT,
    input          JSONB  NOT NULL,
    decided_at_ms  BIGINT NOT NULL
);
