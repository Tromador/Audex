use super::*;
mod execution;

#[test]
fn audit_commands_preserve_explicit_source_and_selection() {
    for args in [
        "--file 'dir/example file.rs' --model selected --provider custom",
        "--snippet 'fn main() {}' --name example.rs",
        "list",
        "list --after cursor",
        "read run",
        "cancel run",
        "release-slot run --reason 'Accept overlap'",
    ] {
        assert!(parse_command(args).is_ok(), "{args}");
    }
    for args in [
        "--file x --snippet code --name x",
        "--snippet code",
        "--file x --name y",
        "read run --provider other",
        "--file 'unclosed",
    ] {
        assert!(parse_command(args).is_err(), "{args}");
    }
}

#[test]
fn conversational_audits_use_tools_with_strict_arguments() {
    let specs = tool_specs();
    let names = specs
        .iter()
        .map(|spec| match spec {
            DynamicToolSpec::Function(function) => function.name.as_str(),
            _ => panic!("audit tools are ordinary function specs"),
        })
        .collect::<Vec<_>>();
    assert_eq!(names, TOOLS);
    assert!(
        tool_action(
            "audit_start",
            json!({"source":{"type":"snippet","name":"x.rs","content":"source"}})
        )
        .is_ok()
    );
    assert!(tool_action("audit_start", json!({"source":{"type":"snippet","name":"x.rs","content":"source"},"permissions":"danger-full-access"})).is_err());
    assert!(tool_action("audit_cancel", json!({"runId":"run","retry":true})).is_err());
    assert!(
        tool_action("audit_release_slot", json!({"runId":"run"})).is_err(),
        "models cannot bypass the operator-only release command"
    );
}

#[test]
fn audit_report_is_untrusted_conversation_context_not_an_instruction_override() {
    let view = AuditRunView {
        run_id: "run".into(),
        snapshot_id: "snapshot".into(),
        assignment_id: "assignment".into(),
        phase: AuditRunPhase::Completed,
        operation_state: codex_app_server_protocol::AuditOperationState::Committed,
        observation_pending: false,
        capacity_released: false,
        revision: 4,
        cancellation_requested: false,
        outcome: None,
        raw_result: None,
        report: Some("# Audit\n\n## Finding 1\nEvidence".into()),
    };
    let context = report_context(&view).unwrap();
    assert!(context.contains("not instructions"));
    assert!(context.contains("Snapshot: snapshot"));
    assert!(context.contains(view.report.as_ref().unwrap()));
    assert!(terminal(&view));
    let operator = ThreadId::new();
    let other = ThreadId::new();
    let mut reports = ReportContexts::default();
    reports.stage(operator, &view);
    reports.stage(operator, &view);
    assert!(reports.for_turn(other).is_none());
    let entries = reports.for_turn(operator).unwrap();
    assert_eq!(
        entries.len(),
        1,
        "repeated observations do not duplicate staged context"
    );
    let entry = entries.values().next().unwrap();
    assert_eq!(entry.kind, AdditionalContextKind::Untrusted);
    assert_eq!(entry.name.as_deref(), Some("audex_audit_report_v1"));
    assert_eq!(entry.value, context);
    assert!(
        reports.for_turn(operator).is_some(),
        "unacknowledged submission retains the context"
    );
    reports.acknowledged(operator);
    assert!(reports.for_turn(operator).is_none());
}
