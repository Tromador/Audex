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
        vec!["audit", "abort", "run-1"],
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
        findings_summary: None,
        collection: Default::default(),
        reviewers: Vec::new(),
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
    assert!(require_protocol_version(AUDIT_PROTOCOL_VERSION - 1).is_err());
    assert!(
        serde_json::from_value::<AuditListResponse>(serde_json::json!({
            "runs":[], "nextCursor":null,
        }))
        .is_err(),
        "an empty legacy list must not bypass compatibility preflight"
    );
}

#[test]
fn published_aggregate_outcomes_override_individual_uncertainty() {
    let mut view = AuditRunView {
        findings_summary: None,
        collection: Default::default(),
        reviewers: Vec::new(),
        run_id: "run".into(),
        snapshot_id: "snapshot".into(),
        assignment_id: "assignment".into(),
        phase: AuditRunPhase::Reporting,
        operation_state: AuditOperationState::Unknown,
        observation_pending: true,
        capacity_released: true,
        revision: 9,
        cancellation_requested: true,
        outcome: None,
        raw_result: None,
        report: None,
    };
    assert_eq!(
        terminal_code(&view),
        None,
        "Abort acknowledgement still awaits publication"
    );
    view.phase = AuditRunPhase::Cancelled;
    view.report = Some("# Aborted report".into());
    view.outcome = Some("aborted_by_operator".into());
    assert_eq!(terminal_code(&view), Some(6));
    view.outcome = Some("no_completed_review".into());
    assert_eq!(terminal_code(&view), Some(3));
    view.phase = AuditRunPhase::Completed;
    view.outcome = Some("success".into());
    assert_eq!(terminal_code(&view), Some(0));
    for state in [
        AuditOperationState::Unknown,
        AuditOperationState::Failed,
        AuditOperationState::Planned,
    ] {
        view.phase = AuditRunPhase::Reviewing;
        view.operation_state = state;
        view.report = None;
        view.outcome = None;
        assert_eq!(terminal_code(&view), None, "wanted work is pending");
    }
    let abort = AuditCommand::try_parse_from(["audit", "abort", "target-run"]).expect("Abort");
    let alias = AuditCommand::try_parse_from(["audit", "cancel", "target-run"])
        .expect("compatibility alias");
    for command in [abort, alias] {
        assert!(matches!(command.action,Some(Action::Abort { run_id }) if run_id=="target-run"));
    }
}

#[test]
fn ordered_roster_sequential_alias_and_exact_attempt_forms_parse() {
    let command = AuditCommand::try_parse_from([
        "audit",
        "--file",
        "example.rs",
        "--reviewer",
        "local:model-a",
        "--reviewer",
        "local:model-a",
        "--sequential",
    ])
    .expect("roster");
    assert_eq!(command.reviewers, vec!["local:model-a", "local:model-a"]);
    assert!(command.sequential);
    assert_eq!(
        parse_reviewer(&command.reviewers[0])
            .expect("reviewer")
            .model,
        "model-a"
    );
    let alias=parse_reviewer(r#"{"provider":"local","model":"alias","expectedServedModel":"model-a","reasoningEffort":"low"}"#).expect("qualified alias");
    assert_eq!(alias.expected_served_model.as_deref(), Some("model-a"));
    assert!(
        parse_reviewer(r#"{"provider":"local","model":"m","executionAdapter":"claude"}"#).is_err()
    );
    for value in ["missing-colon", ":model", "provider:"] {
        assert!(parse_reviewer(value).is_err());
    }
    assert!(
        AuditCommand::try_parse_from(["audit", "--file", "x", "--reviewer", "p:m", "--model", "m"])
            .is_err()
    );
    for action in ["observe", "wait", "cancel", "ignore"] {
        AuditCommand::try_parse_from(["audit", "attempt", action, "run", "exact-assignment"])
            .expect("attempt command");
    }
    assert!(
        AuditCommand::try_parse_from(["audit", "attempt", "retry", "run", "exact-assignment"])
            .is_err()
    );
    AuditCommand::try_parse_from([
        "audit",
        "attempt",
        "retry",
        "run",
        "exact-assignment",
        "--authorise-retry",
        "--acknowledge-possible-overlap",
    ])
    .expect("explicit retry");
}

#[test]
fn typed_reviewer_json_accepts_exact_routes_and_rejects_malformed_pairs() {
    for (executor, route) in [
        ("native", "daemon_configured_native"),
        ("claude_client", "claude_subscription"),
        ("claude_client", "claude_api"),
        ("native", "ollama_local"),
    ] {
        let selection = serde_json::json!({"provider":"p", "model":"m", "executionAdapter":executor, "authenticationRoute":route});
        let reviewer = parse_reviewer(&selection.to_string()).unwrap();
        assert_eq!(reviewer.execution_adapter.to_string(), executor);
        assert_eq!(reviewer.authentication_route.to_string(), route);
    }
    for selection in [
        r#"{"provider":"","model":"m"}"#,
        r#"{"provider":"p","model":"m","executionAdapter":"native","authenticationRoute":"claude_api"}"#,
        r#"{"provider":"p","model":"m","authenticationRoute":"unknown"}"#,
    ] {
        assert!(parse_reviewer(selection).is_err());
    }
    let shorthand = parse_reviewer("p:model:tag").unwrap();
    assert_eq!(shorthand.model, "model:tag");
    assert_eq!(
        shorthand.execution_adapter,
        codex_app_server_protocol::AuditExecutionAdapter::Native
    );
    assert_eq!(
        shorthand.authentication_route,
        codex_app_server_protocol::AuditAuthenticationRoute::DaemonConfiguredNative
    );
}
