-- Run-wide stop generations fence every attempt without sibling revision churn.
ALTER TABLE audit_runs ADD COLUMN stop_generation INTEGER NOT NULL DEFAULT 0 CHECK (stop_generation >= 0);
UPDATE audit_runs SET stop_generation=1 WHERE control_intent<>'active';

-- Admission is irreversible accounting, not proof of a physical send or receipt.
-- These identities and evidence can also be used by an owned API gateway.
CREATE TABLE audit_request_admissions (
    request_id TEXT PRIMARY KEY NOT NULL,
    assignment_id TEXT NOT NULL REFERENCES audit_attempts(assignment_id) ON DELETE RESTRICT,
    operation_id TEXT NOT NULL REFERENCES operations(id) ON DELETE RESTRICT,
    execution_id TEXT NOT NULL,
    owner TEXT NOT NULL,
    owner_generation INTEGER NOT NULL,
    run_stop_generation INTEGER NOT NULL,
    transport TEXT NOT NULL,
    admitted_at_ms INTEGER NOT NULL,
    send_started_at_ms INTEGER,
    confirmed_at_ms INTEGER,
    provider_response_id TEXT
);
CREATE INDEX audit_request_admissions_attempt ON audit_request_admissions(assignment_id,admitted_at_ms,request_id);
