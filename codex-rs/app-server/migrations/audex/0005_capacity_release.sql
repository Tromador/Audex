-- Capacity is independent of execution certainty. This immutable row is also a
-- permanent submission fence; observation cannot remove it.
CREATE TABLE audit_capacity_releases (
    operation_id TEXT PRIMARY KEY NOT NULL REFERENCES operations(id) ON DELETE RESTRICT,
    consent_revision INTEGER NOT NULL,
    reason TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL
);
