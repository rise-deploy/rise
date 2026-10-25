-- What a device login asks for (ADR-0006): `{"kind": "full"}` or a restricted
-- set of project/environment grants. The approver may change it; what they
-- grant is recorded with the approval in `approved_session`. NULL on rows
-- started before the column existed, which ask for full access.
ALTER TABLE device_authorizations ADD COLUMN requested_access JSONB;
