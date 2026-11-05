-- Session-to-account bindings (ADR-0008 section 6). Keep table and column names
-- identical to migrations/postgres/0002_pool_session_binding.sql.
-- No dollar sign anywhere in this file: engines read it as a placeholder marker.
--
-- One row per (provider, session) binding. session_hash is the lowercase hex
-- SHA-256 of the session's selection key, never a raw session id;
-- account_key is the bound AccountKey as serde JSON text; last_used is unix
-- seconds rounded down to the hour.

CREATE TABLE pool_session_binding (
    provider TEXT NOT NULL,
    session_hash TEXT NOT NULL,
    account_key TEXT NOT NULL,
    last_used BIGINT NOT NULL,
    PRIMARY KEY (provider, session_hash)
);

CREATE INDEX pool_session_binding_last_used ON pool_session_binding (last_used);
