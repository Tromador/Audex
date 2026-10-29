use super::*;
use codex_app_server_client::audit::require_audit_protocol_version as require_protocol_version;
use codex_app_server_protocol::{AUDIT_PROTOCOL_VERSION, AuditListResponse};

#[test]
fn source_and_recovery_forms_parse_without_ambient_options() {
    for args in [
        vec![
            "audit",
            "--file",
            "example.rs",
            "--model",
            "review-model",
            "--provider",
            "review-provider",
        ],
        vec![
            "audit",
            "--stdin",
            "--name",
            "example.rs",
            "--audex-md",
            "AUDEX.md",
            "--agents-md",
            "AGENTS.md",
            "--detach",
        ],
        vec!["audit", "read", "run-1", "--follow"],
        vec!["audit", "list", "--limit", "10", "--after", "run-1"],
        vec!["audit", "cancel", "run-1"],
    ] {
        AuditCommand::try_parse_from(args).expect("valid audit form");
    }
    for args in [
        vec!["audit"],
        vec!["audit", "--stdin"],
        vec!["audit", "--name", "example.rs"],
        vec![
            "audit",
            "--file",
            "example.rs",
            "--stdin",
            "--name",
            "example.rs",
        ],
        vec!["audit", "--file", "example.rs", "read", "run-1"],
        vec!["audit", "--file", "example.rs", "--name", "other.rs"],
    ] {
        assert!(
            AuditCommand::try_parse_from(args.iter().copied()).is_err(),
            "{args:?}"
        );
    }
}

#[test]
fn terminal_exit_statuses_do_not_parse_free_text_outcomes() {
    let mut view = AuditRunView {
        run_id: "run".into(),
        snapshot_id: "snapshot".into(),
        assignment_id: "assignment".into(),
        phase: AuditRunPhase::Completed,
        operation_state: AuditOperationState::Committed,
        observation_pending: false,
        revision: 1,
        cancellation_requested: false,
        outcome: Some("arbitrary diagnostic, not a status contract".into()),
        raw_result: None,
        report: Some("# Report\n".into()),
    };
    assert_eq!(terminal_code(&view), Some(0));
    view.phase = AuditRunPhase::Failed;
    view.operation_state = AuditOperationState::Failed;
    assert_eq!(terminal_code(&view), Some(2));
    view.phase = AuditRunPhase::Cancelled;
    assert_eq!(terminal_code(&view), Some(3));
    view.phase = AuditRunPhase::Failed;
    view.operation_state = AuditOperationState::Unknown;
    assert_eq!(terminal_code(&view), Some(4));
    view.phase = AuditRunPhase::Reviewing;
    view.operation_state = AuditOperationState::Accepted;
    view.observation_pending = true;
    assert_eq!(
        terminal_code(&view),
        None,
        "pending observation is not terminal unknown"
    );
}

#[tokio::test]
async fn explicit_input_is_bounded_and_utf8() -> anyhow::Result<()> {
    assert_eq!(bounded_text("abc".as_bytes(), 3).await?, "abc");
    assert!(bounded_text("abcd".as_bytes(), 3).await.is_err());
    assert!(bounded_text([0xff].as_slice(), 3).await.is_err());
    Ok(())
}

#[test]
fn root_runtime_flags_are_rejected_instead_of_ignored() {
    for args in [
        vec![
            "codex",
            "--model",
            "ambient",
            "audit",
            "--file",
            "example.rs",
        ],
        vec![
            "codex",
            "-c",
            "model=\"ambient\"",
            "audit",
            "--file",
            "example.rs",
        ],
        vec!["codex", "--remote", "ws://localhost:1234", "audit", "list"],
        vec!["codex", "--no-daemon", "audit", "list"],
    ] {
        assert!(crate::config_override_args::try_parse_from(args).is_err());
    }
    crate::config_override_args::try_parse_from([
        "codex",
        "audit",
        "--file",
        "example.rs",
        "--model",
        "reviewer",
    ])
    .expect("explicit reviewer flag");
}

#[test]
fn audit_protocol_requires_an_explicit_matching_version_even_when_empty() {
    assert!(require_protocol_version(AUDIT_PROTOCOL_VERSION).is_ok());
    assert!(require_protocol_version(0).is_err());
    assert!(require_protocol_version(AUDIT_PROTOCOL_VERSION + 1).is_err());
    assert!(
        serde_json::from_value::<AuditListResponse>(serde_json::json!({
            "runs":[], "nextCursor":null,
        }))
        .is_err(),
        "an empty legacy list must not bypass compatibility preflight"
    );
}
