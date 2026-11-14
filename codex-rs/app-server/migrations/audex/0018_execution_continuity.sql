-- Observable HTTP/WS send boundaries, including client-managed retries.
-- Internal provider/HTTP-library retries and billing are not inferred from this count.
ALTER TABLE audit_request_admissions ADD COLUMN send_count INTEGER NOT NULL DEFAULT 0;
UPDATE audit_request_admissions SET send_count=1 WHERE send_started_at_ms IS NOT NULL;
ALTER TABLE audit_execution ADD COLUMN owner_process_json TEXT;
ALTER TABLE audit_execution ADD COLUMN stopped_at_ms INTEGER;
ALTER TABLE audit_execution ADD COLUMN recovered_output TEXT;
ALTER TABLE audit_reviewers ADD COLUMN automatic_recovery_used INTEGER NOT NULL DEFAULT 0;
ALTER TABLE audit_attempts ADD COLUMN recovery_status TEXT;
