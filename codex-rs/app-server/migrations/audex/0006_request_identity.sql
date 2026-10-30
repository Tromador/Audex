-- Requested source and reviewer inputs are independent of mutable resolved policy.
ALTER TABLE audit_inputs ADD COLUMN request_identity_digest TEXT;
