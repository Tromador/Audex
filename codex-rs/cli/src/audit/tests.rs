use super::*;
use codex_app_server_client::audit::require_audit_protocol_version as require_protocol_version;
use codex_app_server_protocol::{AUDIT_PROTOCOL_VERSION, AuditListResponse};

#[test]
fn a_server_error_after_durable_start_exposes_recovery_identity() {
    let error = codex_app_server_client::audit::AuditClientError::Request(
        codex_app_server_client::TypedRequestError::Server {
            method: "audit/start".to_owned(),
            source: JSONRPCErrorError {
                code: -32603,
                message: "Response unavailable".to_owned(),
                data: Some(
                    serde_json::json!({"reason":"auditRunRetained","requestKey":"stable-key","runId":"retained-run"}),
                ),
            },
        },
    );
    assert!(error.start_acknowledgement_uncertain());
    assert_eq!(error.retained_run_id(), Some("retained-run"));
    let error = codex_app_server_client::audit::AuditClientError::Request(
        codex_app_server_client::TypedRequestError::Server {
            method: "audit/start".to_owned(),
            source: JSONRPCErrorError {
                code: -32602,
                message: "Invalid input before commit".to_owned(),
                data: None,
            },
        },
    );
    assert!(!error.start_acknowledgement_uncertain());
    assert_eq!(error.retained_run_id(), None);

    let conflict = codex_app_server_client::audit::AuditClientError::Request(
        codex_app_server_client::TypedRequestError::Server {
            method: "audit/start".into(),
            source: JSONRPCErrorError {
                code: -32602,
                message: "Request key reused with different input".into(),
                // Even an unexpected runId must not be advertised as recovery.
                data: Some(
                    serde_json::json!({"reason":"auditIdempotencyConflict","runId":"unrelated"}),
                ),
            },
        },
    );
    assert!(!conflict.start_acknowledgement_uncertain());
    assert_eq!(conflict.retained_run_id(), None);
}

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
        vec![
            "audit",
            "release-slot",
            "run-1",
            "--acknowledge-concurrent-execution-and-charges",
            "--reason",
            "Accept overlap",
        ],
    ] {
        AuditCommand::try_parse_from(args).expect("valid audit form");
    }
    for args in [
        vec!["audit"],
        vec![
            "audit",
            "release-slot",
            "run-1",
            "--reason",
            "No acknowledgement",
        ],
        vec![
            "audit",
            "release-slot",
            "run-1",
            "--acknowledge-concurrent-execution-and-charges",
        ],
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
        capacity_released: false,
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
