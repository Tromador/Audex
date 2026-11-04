-- Operational receipts remain outside accepted contributions and frozen reports.
ALTER TABLE audit_client_launches ADD COLUMN daemon_identity_json TEXT;
ALTER TABLE audit_client_launches ADD COLUMN authority_json TEXT;
ALTER TABLE audit_client_launches ADD COLUMN ownership_json TEXT NOT NULL DEFAULT '{}';
ALTER TABLE audit_client_launches ADD COLUMN local_stop_requested_at_ms INTEGER;
ALTER TABLE audit_client_launches ADD COLUMN local_stop_confirmed_at_ms INTEGER;
ALTER TABLE audit_client_launches ADD COLUMN cleanup_finished_at_ms INTEGER;
ALTER TABLE audit_client_launches ADD COLUMN cleanup_checked_at_ms INTEGER;
ALTER TABLE audit_client_launches ADD COLUMN local_status TEXT NOT NULL DEFAULT 'uninspected';
CREATE INDEX audit_client_cleanup ON audit_client_launches(cleanup_finished_at_ms);
