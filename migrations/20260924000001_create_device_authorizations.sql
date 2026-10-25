-- Pending `rise login --device` logins (RFC 8628, with Rise as the
-- authorization server).
--
-- The CLI holds the device code and polls with it; the user confirms the
-- short user code on Rise's `/device` page with a fresh session, which records
-- that session's User and UserIdentity here. The next poll consumes the row
-- and receives a session bound to the same identity. Only a hash of the device
-- code is stored, so a leaked row cannot be redeemed.
CREATE TABLE device_authorizations (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    device_code_hash BYTEA NOT NULL UNIQUE,
    user_code TEXT NOT NULL UNIQUE,
    status TEXT NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending', 'approved', 'denied')),
    -- The typed user and the session identity that approved the login; set
    -- exactly when status is 'approved'.
    approved_user_id UUID REFERENCES users(id) ON DELETE CASCADE,
    approved_session JSONB,
    -- Shown on the confirmation page so the user can tell the request apart
    -- from one somebody else started.
    client_name TEXT,
    client_ip TEXT,
    interval_seconds INTEGER NOT NULL,
    last_polled_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at TIMESTAMPTZ NOT NULL,
    CONSTRAINT device_authorizations_approval_complete CHECK (
        (status = 'approved') = (approved_user_id IS NOT NULL AND approved_session IS NOT NULL)
    )
);

CREATE INDEX idx_device_authorizations_expires_at ON device_authorizations (expires_at);
