-- Store schema v1 (ADR-0006). Keep table and column names identical to
-- migrations/postgres/0001_init.sql; only types differ where an engine needs it.
-- No dollar sign anywhere in this file: engines read it as a placeholder marker.

CREATE TABLE import_record (
    subsystem TEXT PRIMARY KEY,
    imported_at BIGINT NOT NULL,
    source_path TEXT NULL,
    source_sha256 TEXT NULL,
    row_count BIGINT NOT NULL
);

-- IF NOT EXISTS: the hold table is also created by `ensure_hold_table` before
-- migrations run, so a refused opener never migrates a live holder's schema.
CREATE TABLE IF NOT EXISTS instance_hold (
    id BIGINT PRIMARY KEY CHECK (id = 1),
    holder_id TEXT NULL,
    holder_label TEXT NULL,
    started_at BIGINT NULL,
    heartbeat_at BIGINT NULL
);

INSERT INTO instance_hold (id) VALUES (1) ON CONFLICT DO NOTHING;

CREATE TABLE pool_quota (
    account_key TEXT PRIMARY KEY,
    quota_json TEXT NOT NULL,
    updated_at BIGINT NOT NULL
);

CREATE TABLE refresh_token (
    token_sha256 TEXT PRIMARY KEY,
    family TEXT NOT NULL,
    identity_json TEXT NOT NULL,
    inactive_since BIGINT NULL,
    issued_at BIGINT NOT NULL
);

CREATE INDEX refresh_token_family ON refresh_token (family);

CREATE TABLE spend_limit (
    ord BIGINT PRIMARY KEY,
    id TEXT NULL,
    opaque INTEGER NOT NULL,
    body TEXT NOT NULL
);

CREATE TABLE spend_audit (
    ord BIGINT PRIMARY KEY,
    audit_id BIGINT NULL,
    opaque INTEGER NOT NULL,
    body TEXT NOT NULL
);

CREATE TABLE spend_meta (
    id BIGINT PRIMARY KEY CHECK (id = 1),
    next_audit_id BIGINT NOT NULL
);
