-- Control disposition and received output are distinct from execution certainty.
ALTER TABLE audit_attempts ADD COLUMN disposition TEXT NOT NULL DEFAULT 'active'
    CHECK (disposition IN ('active','cancelled','ignored','superseded'));
ALTER TABLE audit_attempts ADD COLUMN output_sequence INTEGER NOT NULL DEFAULT 0 CHECK (output_sequence >= 0);
ALTER TABLE audit_attempts ADD COLUMN output_cutoff INTEGER CHECK (output_cutoff >= 0 AND output_cutoff <= output_sequence);
ALTER TABLE audit_attempts ADD COLUMN output_bytes INTEGER NOT NULL DEFAULT 0 CHECK (output_bytes >= 0);
ALTER TABLE audit_attempts ADD COLUMN capture_started INTEGER NOT NULL DEFAULT 0 CHECK (capture_started IN (0,1));
ALTER TABLE audit_attempts ADD COLUMN capture_finished INTEGER NOT NULL DEFAULT 0 CHECK (capture_finished IN (0,1));
ALTER TABLE audit_attempts ADD COLUMN capture_gap TEXT;
ALTER TABLE audit_attempts ADD COLUMN capture_failure TEXT;
ALTER TABLE audit_attempts ADD COLUMN capture_truncated INTEGER NOT NULL DEFAULT 0 CHECK (capture_truncated IN (0,1));
UPDATE audit_attempts SET capture_gap='activity before capture support was not retained'
WHERE assignment_id IN (SELECT assignment_id FROM operations WHERE submission_boundary<>'unsubmitted');

CREATE TABLE audit_attempt_actions (
    action_key TEXT PRIMARY KEY NOT NULL,
    assignment_id TEXT NOT NULL REFERENCES audit_attempts(assignment_id) ON DELETE RESTRICT,
    request_digest TEXT NOT NULL,
    response_json TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL
);

CREATE TABLE audit_activity (
    assignment_id TEXT NOT NULL REFERENCES audit_attempts(assignment_id) ON DELETE RESTRICT,
    sequence INTEGER NOT NULL CHECK (sequence > 0),
    producer_key TEXT NOT NULL,
    session_id TEXT NOT NULL,
    turn_id TEXT NOT NULL,
    item_id TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('assistant_delta','assistant_message','tool_start','tool_result','status','gap','truncation')),
    payload TEXT NOT NULL,
    payload_digest TEXT NOT NULL,
    source_digest TEXT NOT NULL,
    record_digest TEXT NOT NULL,
    text_offset INTEGER CHECK (text_offset >= 0),
    created_at_ms INTEGER NOT NULL,
    PRIMARY KEY (assignment_id, sequence),
    UNIQUE (assignment_id, producer_key)
);
-- Public assistant messages have one canonical text projection. Complete items
-- add only the suffix not already captured as deltas; repeated prose is retained.
CREATE TABLE audit_activity_items (
    assignment_id TEXT NOT NULL REFERENCES audit_attempts(assignment_id) ON DELETE RESTRICT,
    session_id TEXT NOT NULL,
    turn_id TEXT NOT NULL,
    item_id TEXT NOT NULL,
    text_bytes INTEGER NOT NULL CHECK (text_bytes >= 0),
    complete INTEGER NOT NULL CHECK (complete IN (0,1)),
    PRIMARY KEY (assignment_id, session_id, turn_id, item_id)
);
CREATE INDEX audit_activity_cursor ON audit_activity(assignment_id, sequence);
