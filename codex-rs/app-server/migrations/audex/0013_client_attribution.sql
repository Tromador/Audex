-- Client observations are not admissions. Stable messages belong to one attempt
-- and its reserved execution, independently of the strict generation ledger.
CREATE TABLE audit_client_model_observations (
    assignment_id TEXT NOT NULL REFERENCES audit_attempts(assignment_id) ON DELETE RESTRICT,
    operation_id TEXT NOT NULL REFERENCES operations(id) ON DELETE RESTRICT,
    execution_id TEXT NOT NULL,
    message_id TEXT NOT NULL,
    expected_model TEXT NOT NULL,
    observed_model TEXT,
    identity_state TEXT NOT NULL CHECK (identity_state IN ('verified','missing','mismatch','unverified')),
    PRIMARY KEY (assignment_id, execution_id, message_id)
);
CREATE TABLE audit_client_terminal_evidence (
    assignment_id TEXT PRIMARY KEY NOT NULL REFERENCES audit_attempts(assignment_id) ON DELETE RESTRICT,
    operation_id TEXT NOT NULL REFERENCES operations(id) ON DELETE RESTRICT,
    execution_id TEXT NOT NULL,
    terminal_id TEXT NOT NULL,
    successful INTEGER NOT NULL CHECK (successful IN (0,1)),
    verified INTEGER NOT NULL CHECK (verified IN (0,1)),
    result_digest TEXT NOT NULL
);
