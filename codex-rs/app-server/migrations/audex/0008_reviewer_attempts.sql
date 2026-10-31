-- Terminal legacy history remains readable. Never guess how to resume old work.
-- This guard also runs under the migration transaction, closing the preflight race.
CREATE TABLE audex_legacy_upgrade_guard (
    nonterminal_runs INTEGER CONSTRAINT unsupported_nonterminal_legacy_audit CHECK (nonterminal_runs = 0)
);
INSERT INTO audex_legacy_upgrade_guard
SELECT count(*) FROM audit_runs
WHERE phase NOT IN ('completed', 'completed_with_failures', 'failed', 'cancelled');
DROP TABLE audex_legacy_upgrade_guard;

-- A selected reviewer is distinct from its execution attempts. Assignment ordinals
-- remain unique within a run; attempt ordering belongs to the selected reviewer.
CREATE TABLE audit_reviewers (
    id TEXT PRIMARY KEY NOT NULL,
    run_id TEXT NOT NULL REFERENCES audit_runs(id) ON DELETE RESTRICT,
    ordinal INTEGER NOT NULL CHECK (ordinal >= 0),
    current_assignment_id TEXT NOT NULL UNIQUE,
    UNIQUE (run_id, ordinal),
    UNIQUE (id, run_id),
    FOREIGN KEY (id, current_assignment_id)
        REFERENCES audit_attempts(reviewer_id, assignment_id) DEFERRABLE INITIALLY DEFERRED
);

CREATE UNIQUE INDEX reviewer_assignments_identity_run ON reviewer_assignments(id, run_id);

CREATE TABLE audit_attempts (
    assignment_id TEXT PRIMARY KEY NOT NULL REFERENCES reviewer_assignments(id) ON DELETE RESTRICT,
    reviewer_id TEXT NOT NULL REFERENCES audit_reviewers(id) ON DELETE RESTRICT,
    run_id TEXT NOT NULL,
    ordinal INTEGER NOT NULL CHECK (ordinal >= 1),
    predecessor_assignment_id TEXT UNIQUE REFERENCES audit_attempts(assignment_id) ON DELETE RESTRICT,
    policy_digest TEXT NOT NULL,
    revision INTEGER NOT NULL DEFAULT 0 CHECK (revision >= 0),
    owner_generation INTEGER NOT NULL DEFAULT 0 CHECK (owner_generation >= 0),
    UNIQUE (reviewer_id, ordinal),
    UNIQUE (reviewer_id, assignment_id),
    FOREIGN KEY (assignment_id, run_id) REFERENCES reviewer_assignments(id, run_id),
    FOREIGN KEY (reviewer_id, run_id) REFERENCES audit_reviewers(id, run_id),
    FOREIGN KEY (reviewer_id, predecessor_assignment_id) REFERENCES audit_attempts(reviewer_id, assignment_id)
);

-- Non-secret provider-correct input and resolved policy are owned by the attempt.
-- Runtime CatalogModel values continue to be rebuilt by the existing native path.
CREATE TABLE audit_attempt_inputs (
    assignment_id TEXT PRIMARY KEY NOT NULL REFERENCES audit_attempts(assignment_id) ON DELETE RESTRICT,
    request_json TEXT NOT NULL,
    input_digest TEXT NOT NULL,
    policy_json TEXT NOT NULL,
    policy_digest TEXT NOT NULL
);

INSERT INTO audit_reviewers (id, run_id, ordinal, current_assignment_id)
SELECT 'reviewer:' || id, run_id, ordinal, id FROM reviewer_assignments;
INSERT INTO audit_attempts (assignment_id, reviewer_id, run_id, ordinal, revision, policy_digest)
SELECT a.id, 'reviewer:' || a.id, a.run_id, 1, r.revision, r.policy_digest
FROM reviewer_assignments a JOIN audit_runs r ON r.id=a.run_id;
INSERT INTO audit_attempt_inputs (assignment_id, request_json, input_digest, policy_json, policy_digest)
SELECT a.id, i.request_json, r.input_digest, i.policy_json, r.policy_digest
FROM reviewer_assignments a JOIN audit_runs r ON r.id=a.run_id
JOIN audit_inputs i ON i.run_id=r.id;

CREATE INDEX audit_attempts_reviewer ON audit_attempts(reviewer_id, ordinal);
