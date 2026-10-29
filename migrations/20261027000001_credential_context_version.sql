CREATE TABLE credential_context_migrations (
    version INTEGER PRIMARY KEY,
    provider_canary TEXT,
    completed_at TIMESTAMPTZ,
    CHECK (provider_canary IS NOT NULL OR completed_at IS NOT NULL)
);

CREATE TABLE credential_context_migration_progress (
    version INTEGER NOT NULL,
    record_key TEXT NOT NULL,
    fingerprint TEXT NOT NULL,
    PRIMARY KEY (version, record_key)
);
