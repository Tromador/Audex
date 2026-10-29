-- Reservation is not submission. Only new operations can prove that the core
-- turn API has never been called. Preserve uncertainty for older dispatched work.
ALTER TABLE operations ADD COLUMN submission_boundary TEXT NOT NULL
    DEFAULT 'legacy_uncertain'
    CHECK (submission_boundary IN ('unsubmitted', 'submission_started', 'accepted', 'legacy_uncertain'));
UPDATE operations SET submission_boundary='unsubmitted' WHERE state='planned';
UPDATE operations SET submission_boundary='accepted'
WHERE state IN ('accepted','observed_terminal','committed')
   OR id IN (SELECT operation_id FROM audit_execution WHERE turn_id IS NOT NULL);
