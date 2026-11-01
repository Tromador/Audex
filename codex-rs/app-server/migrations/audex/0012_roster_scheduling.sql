-- Per-audit scheduling is a retained operator choice, using the existing claims.
ALTER TABLE audit_runs ADD COLUMN scheduling TEXT NOT NULL DEFAULT 'automatic'
    CHECK (scheduling IN ('automatic','sequential'));

-- Per-generation attribution is evidence, separate from received output capture.
ALTER TABLE audit_attempts ADD COLUMN attribution_failure TEXT;
ALTER TABLE audit_request_admissions ADD COLUMN expected_model TEXT;
ALTER TABLE audit_request_admissions ADD COLUMN observed_model TEXT;
ALTER TABLE audit_request_admissions ADD COLUMN model_scope TEXT CHECK(model_scope IN ('connection','generation'));
ALTER TABLE audit_request_admissions ADD COLUMN identity_state TEXT NOT NULL DEFAULT 'legacy'
    CHECK(identity_state IN ('legacy','pending','observed','verified','missing','mismatch'));
CREATE TABLE audit_model_observations (
    request_id TEXT NOT NULL REFERENCES audit_request_admissions(request_id),
    ordinal INTEGER NOT NULL,
    model TEXT NOT NULL,
    evidence_source TEXT NOT NULL,
    PRIMARY KEY(request_id,ordinal)
);
