-- Extend the existing launch and client evidence, not the attempt journal authority.
ALTER TABLE audit_client_launches ADD COLUMN executable_digest TEXT;
ALTER TABLE audit_client_launches ADD COLUMN client_session_id TEXT;
ALTER TABLE audit_client_launches ADD COLUMN client_accepted_at_ms INTEGER;
CREATE UNIQUE INDEX audit_client_session_identity ON audit_client_launches(client_session_id)
    WHERE client_session_id IS NOT NULL;
ALTER TABLE audit_client_model_observations ADD COLUMN client_session_id TEXT;
ALTER TABLE audit_client_model_observations ADD COLUMN usage_json TEXT;
ALTER TABLE audit_client_terminal_evidence ADD COLUMN client_session_id TEXT;
ALTER TABLE audit_client_terminal_evidence ADD COLUMN final_message_id TEXT;
ALTER TABLE audit_client_terminal_evidence ADD COLUMN artefact_path TEXT;
ALTER TABLE audit_client_terminal_evidence ADD COLUMN artefact_digest TEXT;
ALTER TABLE audit_client_terminal_evidence ADD COLUMN usage_json TEXT;
ALTER TABLE reviewer_assignments ADD COLUMN client_terminal_digest TEXT;
