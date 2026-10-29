-- Host-owned identity survives the acceptance/ledger crash window. Neither
-- thread creation nor this binding is evidence that a turn was accepted.
CREATE TABLE audit_execution (
    operation_id TEXT PRIMARY KEY NOT NULL REFERENCES operations(id) ON DELETE RESTRICT,
    thread_id TEXT NOT NULL UNIQUE,
    turn_id TEXT,
    policy_digest TEXT NOT NULL
);
