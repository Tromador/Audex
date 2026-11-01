-- Readiness is irreversible and precedes rendering. Historical terminal runs
-- retain their original outcome and report without conversion.
ALTER TABLE audit_runs ADD COLUMN collection_ready INTEGER NOT NULL DEFAULT 0 CHECK (collection_ready IN (0,1));
ALTER TABLE audit_runs ADD COLUMN selection_generation INTEGER NOT NULL DEFAULT 0;
ALTER TABLE audit_runs ADD COLUMN authoritative_selection_id TEXT REFERENCES audit_report_selections(id);
CREATE TABLE audit_report_selections (
    id TEXT PRIMARY KEY NOT NULL,
    run_id TEXT NOT NULL REFERENCES audit_runs(id),
    generation INTEGER NOT NULL,
    input_json TEXT NOT NULL,
    input_digest TEXT NOT NULL,
    UNIQUE(run_id,generation)
);
UPDATE audit_runs SET collection_ready=1 WHERE phase IN ('reporting','completed','completed_with_failures','failed','cancelled') OR current_report_id IS NOT NULL;
UPDATE audit_runs SET collection_ready=1,phase='reporting',outcome=NULL,revision=revision+1
WHERE current_report_id IS NULL AND control_intent='active'
AND phase IN ('created','snapshot_bound','reviewing')
AND EXISTS(SELECT 1 FROM audit_reviewers v WHERE v.run_id=audit_runs.id)
AND NOT EXISTS(SELECT 1 FROM audit_reviewers v JOIN audit_attempts t ON t.assignment_id=v.current_assignment_id
    JOIN reviewer_assignments a ON a.id=t.assignment_id
    WHERE v.run_id=audit_runs.id AND t.disposition='active' AND a.state<>'succeeded');

-- An acknowledged pre-upgrade audit-wide cancel is already a durable stop.
-- Upgrade only unfinished runs: preserve completed peers and the durable prefix,
-- then recover local Abort publication without replaying reviewer execution.
UPDATE audit_attempts SET
    capture_gap=CASE WHEN capture_started=1 AND capture_finished=0 THEN COALESCE(capture_gap,'collector unavailable during upgrade of audit-wide stop; uncommitted tail may be missing') ELSE capture_gap END,
    output_cutoff=COALESCE(output_cutoff,output_sequence),capture_finished=1,
    disposition=CASE WHEN disposition='active' THEN 'cancelled' ELSE disposition END,
    revision=revision+1,owner_generation=owner_generation+1
WHERE assignment_id IN (
    SELECT v.current_assignment_id FROM audit_reviewers v JOIN audit_runs r ON r.id=v.run_id
    JOIN reviewer_assignments a ON a.id=v.current_assignment_id
    WHERE r.control_intent='cancel_requested' AND r.current_report_id IS NULL
    AND r.phase NOT IN ('completed','completed_with_failures','failed','cancelled') AND a.state<>'succeeded'
);
INSERT INTO audit_capacity_releases(operation_id,consent_revision,reason,created_at_ms)
SELECT o.id,t.revision,'recovered whole-audit Abort',r.updated_at_ms
FROM audit_runs r JOIN audit_reviewers v ON v.run_id=r.id JOIN audit_attempts t ON t.assignment_id=v.current_assignment_id
JOIN reviewer_assignments a ON a.id=t.assignment_id JOIN operations o ON o.assignment_id=a.id
WHERE r.control_intent='cancel_requested' AND r.current_report_id IS NULL
AND r.phase NOT IN ('completed','completed_with_failures','failed','cancelled') AND a.state<>'succeeded'
ON CONFLICT(operation_id) DO NOTHING;
UPDATE audit_runs SET collection_ready=1,phase='reporting',outcome=NULL,stop_generation=stop_generation+1,revision=revision+1
WHERE control_intent='cancel_requested' AND current_report_id IS NULL
AND phase NOT IN ('completed','completed_with_failures','failed','cancelled');
