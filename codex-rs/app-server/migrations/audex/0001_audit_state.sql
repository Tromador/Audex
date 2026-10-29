CREATE TABLE audit_runs (
    id TEXT PRIMARY KEY NOT NULL,
    request_key TEXT NOT NULL UNIQUE,
    snapshot_id TEXT NOT NULL,
    manifest_digest TEXT NOT NULL,
    manifest_path TEXT NOT NULL,
    brief_digest TEXT NOT NULL,
    policy_digest TEXT NOT NULL,
    input_digest TEXT NOT NULL,
    phase TEXT NOT NULL CHECK (phase IN (
        'created', 'snapshot_bound', 'reviewing', 'reporting', 'reconciling',
        'completed', 'completed_with_failures', 'failed', 'cancelled'
    )),
    control_intent TEXT NOT NULL CHECK (control_intent IN ('active', 'cancel_requested')),
    revision INTEGER NOT NULL CHECK (revision >= 0),
    outcome TEXT,
    current_report_id TEXT,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL
);

CREATE TABLE reviewer_assignments (
    id TEXT PRIMARY KEY NOT NULL,
    run_id TEXT NOT NULL REFERENCES audit_runs(id) ON DELETE RESTRICT,
    ordinal INTEGER NOT NULL CHECK (ordinal >= 0),
    snapshot_id TEXT NOT NULL,
    provider TEXT NOT NULL,
    model TEXT NOT NULL,
    reasoning_effort TEXT,
    reviewer_input_digest TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN (
        'planned', 'thread_ready', 'dispatching', 'running', 'succeeded',
        'failed', 'cancelled', 'unknown'
    )),
    result_digest TEXT,
    result_path TEXT,
    result_schema_version INTEGER,
    summary TEXT,
    failure_code TEXT,
    usage_json TEXT,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    UNIQUE (run_id, ordinal)
);

CREATE TABLE operations (
    id TEXT PRIMARY KEY NOT NULL,
    assignment_id TEXT NOT NULL UNIQUE REFERENCES reviewer_assignments(id) ON DELETE RESTRICT,
    kind TEXT NOT NULL CHECK (kind IN ('review')),
    state TEXT NOT NULL CHECK (state IN (
        'planned', 'dispatching', 'accepted', 'observed_terminal',
        'committed', 'failed', 'unknown'
    )),
    attempt INTEGER NOT NULL CHECK (attempt >= 0),
    input_digest TEXT NOT NULL,
    lease_owner TEXT,
    lease_expires_at_ms INTEGER,
    acceptance_evidence TEXT,
    retry_classification TEXT,
    error TEXT,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    CHECK ((lease_owner IS NULL) = (lease_expires_at_ms IS NULL))
);

CREATE TABLE findings (
    id TEXT PRIMARY KEY NOT NULL,
    assignment_id TEXT NOT NULL REFERENCES reviewer_assignments(id) ON DELETE RESTRICT,
    ordinal INTEGER NOT NULL CHECK (ordinal >= 0),
    severity TEXT NOT NULL CHECK (severity IN ('info', 'low', 'medium', 'high', 'critical')),
    confidence TEXT NOT NULL CHECK (confidence IN ('low', 'medium', 'high')),
    title TEXT NOT NULL,
    mechanism TEXT NOT NULL,
    impact TEXT NOT NULL,
    remediation TEXT,
    verification_state TEXT NOT NULL CHECK (verification_state IN ('unverified_model_claim')),
    UNIQUE (assignment_id, ordinal)
);

CREATE TABLE evidence (
    id TEXT PRIMARY KEY NOT NULL,
    finding_id TEXT NOT NULL REFERENCES findings(id) ON DELETE RESTRICT,
    ordinal INTEGER NOT NULL CHECK (ordinal >= 0),
    description TEXT NOT NULL,
    manifest_path TEXT NOT NULL,
    start_byte INTEGER NOT NULL CHECK (start_byte >= 0),
    end_byte INTEGER NOT NULL CHECK (end_byte > start_byte),
    start_line INTEGER,
    end_line INTEGER,
    UNIQUE (finding_id, ordinal),
    CHECK ((start_line IS NULL) = (end_line IS NULL)),
    CHECK (start_line IS NULL OR (start_line > 0 AND end_line >= start_line))
);

CREATE TABLE disclosures (
    id TEXT PRIMARY KEY NOT NULL,
    assignment_id TEXT NOT NULL REFERENCES reviewer_assignments(id) ON DELETE RESTRICT,
    tool_call_id TEXT NOT NULL,
    snapshot_id TEXT NOT NULL,
    manifest_path TEXT NOT NULL,
    entry_digest TEXT NOT NULL,
    start_byte INTEGER NOT NULL CHECK (start_byte >= 0),
    end_byte INTEGER NOT NULL CHECK (end_byte >= start_byte),
    created_at_ms INTEGER NOT NULL
);

CREATE TABLE reports (
    id TEXT PRIMARY KEY NOT NULL,
    run_id TEXT NOT NULL REFERENCES audit_runs(id) ON DELETE RESTRICT,
    format_version INTEGER NOT NULL,
    input_digest TEXT NOT NULL,
    relative_path TEXT NOT NULL,
    content_digest TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL,
    UNIQUE (run_id, input_digest)
);

CREATE TABLE lifecycle_observations (
    idempotency_key TEXT PRIMARY KEY NOT NULL,
    assignment_id TEXT NOT NULL REFERENCES reviewer_assignments(id) ON DELETE RESTRICT,
    kind TEXT NOT NULL,
    payload_digest TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL
);

CREATE INDEX audit_runs_phase ON audit_runs(phase);
CREATE INDEX reviewer_assignments_run ON reviewer_assignments(run_id, ordinal);
CREATE INDEX operations_state ON operations(state);
CREATE INDEX findings_assignment ON findings(assignment_id, ordinal);
CREATE INDEX evidence_finding ON evidence(finding_id, ordinal);
CREATE INDEX reports_run ON reports(run_id);
