-- Restart needs the exact logical request and non-secret prepared reviewer policy.
CREATE TABLE audit_inputs (
    run_id TEXT PRIMARY KEY NOT NULL REFERENCES audit_runs(id) ON DELETE RESTRICT,
    request_json TEXT NOT NULL,
    policy_json TEXT NOT NULL
);
