-- Step 4, slice S3b: soft build provenance, alias-only Project merge and
-- GitHub App linking.

-- Soft provenance (Q18). A device is `self_built` until it presents a build
-- statement that verifies against a configured CI public key. The tag is
-- soft: a statement can be copied into a fork.
ALTER TABLE devices
    ADD COLUMN build_tag TEXT NOT NULL DEFAULT 'self_built'
        CHECK (build_tag IN ('ci_signed', 'self_built')),
    ADD COLUMN build_version TEXT,
    ADD COLUMN build_git_commit TEXT,
    ADD COLUMN build_target TEXT;

-- The tag a Pairing carries until the user approves it and the device exists.
ALTER TABLE pairings
    ADD COLUMN build_tag TEXT NOT NULL DEFAULT 'self_built'
        CHECK (build_tag IN ('ci_signed', 'self_built')),
    ADD COLUMN build_version TEXT,
    ADD COLUMN build_git_commit TEXT,
    ADD COLUMN build_target TEXT;

-- Project merge (Q17): alias-only. Buckets are never rewritten; every read
-- resolves `from_id` to its chain's final `to_id`. Both ends belong to one
-- user (enforced by the API; the composite key keeps aliases per user).
CREATE TABLE project_aliases (
    user_id       TEXT   NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    from_id       TEXT   NOT NULL,
    to_id         TEXT   NOT NULL,
    created_at_ms BIGINT NOT NULL,
    PRIMARY KEY (user_id, from_id),
    CHECK (from_id <> to_id)
);

-- GitHub linking (Q16). The user's GitHub login, learned from a
-- user-to-server token; the token itself is never stored.
CREATE TABLE github_accounts (
    user_id      TEXT   PRIMARY KEY REFERENCES users (id) ON DELETE CASCADE,
    github_id    BIGINT NOT NULL,
    login        TEXT   NOT NULL,
    linked_at_ms BIGINT NOT NULL
);

-- A Project linked to a repo the user has push access to. The commits live
-- in `project_commits` (oid and timestamps only, written by the GitHub
-- client); this row holds what the fetch needs.
CREATE TABLE linked_repos (
    user_id         TEXT   NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    project_id      TEXT   NOT NULL,
    github_repo_id  BIGINT NOT NULL,
    full_name       TEXT   NOT NULL,
    installation_id BIGINT NOT NULL,
    linked_at_ms    BIGINT NOT NULL,
    last_fetch_ms   BIGINT,
    PRIMARY KEY (user_id, project_id)
);
