-- Server-side state for OAuth extensions in `rotating` refresh-token mode.
--
-- A *family* is the chain of refresh handles descending from one login. Every
-- refresh consumes one handle and issues its successor in the same family.
-- Revoking a family is a single-row update, so it takes effect for every
-- handle in the chain, including a successor that is being issued concurrently.
CREATE TABLE oauth_refresh_families (
    family_id UUID PRIMARY KEY,
    project_id UUID NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    extension VARCHAR NOT NULL,
    -- Fingerprint of the upstream (issuer_url, token_endpoint override,
    -- client_id) the family's upstream token was issued by. Handles of the
    -- family are refused once the extension is configured differently.
    upstream_identity TEXT NOT NULL,
    -- Absolute end of the family: creation + the extension's max lifetime.
    expires_at TIMESTAMPTZ NOT NULL,
    revoked_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    FOREIGN KEY (project_id, extension)
        REFERENCES project_extensions(project_id, extension) ON DELETE CASCADE
);

CREATE INDEX idx_oauth_refresh_families_expires_at ON oauth_refresh_families(expires_at);
CREATE INDEX idx_oauth_refresh_families_extension ON oauth_refresh_families(project_id, extension);

-- One row per issued handle, keyed by the SHA-256 of the handle: the handle
-- itself is never stored.
CREATE TABLE oauth_refresh_tokens (
    handle_hash BYTEA PRIMARY KEY CHECK (octet_length(handle_hash) = 32),
    family_id UUID NOT NULL REFERENCES oauth_refresh_families(family_id) ON DELETE CASCADE,
    project_id UUID NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    extension VARCHAR NOT NULL,
    -- Upstream refresh token, bound to this row's handle hash. Cleared once the
    -- handle is rotated.
    upstream_token_encrypted TEXT,
    -- Claim lease held while the winner of a refresh talks to the upstream.
    claimed_by UUID,
    claim_expires_at TIMESTAMPTZ,
    -- Set when the handle has been exchanged for its successor.
    rotated_at TIMESTAMPTZ,
    grace_until TIMESTAMPTZ,
    -- Exact token response returned for the rotation, replayed to duplicate
    -- requests within the grace window and cleared after it.
    successor_response_encrypted TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CHECK ((rotated_at IS NULL) = (grace_until IS NULL)),
    CHECK (rotated_at IS NULL OR upstream_token_encrypted IS NULL)
);

CREATE INDEX idx_oauth_refresh_tokens_family ON oauth_refresh_tokens(family_id);
CREATE INDEX idx_oauth_refresh_tokens_extension ON oauth_refresh_tokens(project_id, extension);
CREATE INDEX idx_oauth_refresh_tokens_grace_until ON oauth_refresh_tokens(grace_until)
    WHERE successor_response_encrypted IS NOT NULL;
