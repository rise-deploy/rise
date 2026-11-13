-- Transient key-value store behind the OAuth relay (`oauthrelay_core::TransientStore`):
-- single-use replay markers and rotating refresh-token families.
--
-- Keys are namespaced by the caller (`replay:`, `refresh:`, `rise:oauth-test:`) and
-- values are opaque bytes, sealed by the relay. Expiry is judged by the database
-- clock on every read and conditional write, so a row past `expires_at` is absent
-- even before the hourly cleanup removes it.
CREATE TABLE oauth_relay_store (
    key        TEXT        PRIMARY KEY CHECK (octet_length(key) BETWEEN 1 AND 1024),
    value      BYTEA       NOT NULL,
    expires_at TIMESTAMPTZ NOT NULL
);

CREATE INDEX oauth_relay_store_expires_at_idx ON oauth_relay_store (expires_at);
