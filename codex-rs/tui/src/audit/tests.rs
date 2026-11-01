use super::*;
mod execution;
#[cfg(unix)]
mod process;

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
        collection: Default::default(),
        reviewers: Vec::new(),
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

pub(crate) fn attempt_fixture() -> codex_app_server_protocol::AuditAttemptView {
    serde_json::from_value(json!({"runId":"run-1","reviewerId":"reviewer-1","reviewerOrdinal":0,"assignmentId":"assignment-original","attemptOrdinal":1,
        "operationId":"operation-1","snapshotId":"snapshot","provider":"native-provider","model":"native-model","requestedModel":"native-model","resolvedModel":"native-model",
        "expectedServedModel":null,"reasoningEffort":null,"revision":1,"ownerGeneration":1,"current":true,"retryEligible":true,"executionAdapter":"native",
        "authenticationRoute":"daemon_configured_native","modelEvidence":[],"disposition":"active","assignmentState":"running","operationState":"accepted","completed":false,
        "possibleOverlap":true,"outputSequence":3,"outputCutoff":null,"captureGap":null,"captureFailure":null,"captureTruncated":false})).unwrap()
}

#[tokio::test]
async fn ordered_roster_and_exact_attempt_commands_preserve_operator_choices() {
    let command=parse_command("--snippet code --name x.rs --reviewer p:same --reviewer p:same --reviewer '{\"provider\":\"q\",\"model\":\"alias\",\"expectedServedModel\":\"served\"}' --sequential --request-key stable").unwrap().action(std::path::Path::new("/")).await.unwrap();
    let AuditAction::Start(args) = command else {
        panic!("start")
    };
    assert_eq!(
        args.scheduling,
        codex_app_server_protocol::AuditScheduling::Sequential
    );
    assert_eq!(args.request_key.as_deref(), Some("stable"));
    let roster = args.reviewers.unwrap();
    assert_eq!(roster.len(), 3);
    assert_eq!(roster[0], roster[1]);
    assert_eq!(roster[2].expected_served_model.as_deref(), Some("served"));
    assert!(parse_command("--file x --reviewer p:m --model another").is_err());
    for spelling in ["abort run-1", "cancel run-1"] {
        assert!(
            matches!(parse_command(spelling).unwrap().action(std::path::Path::new("/")).await.unwrap(),AuditAction::Abort(id) if id=="run-1")
        );
    }
    let action = parse_command("attempt retry run-1 assignment-original --action-key same-action")
        .unwrap()
        .action(std::path::Path::new("/"))
        .await
        .unwrap();
    assert!(
        matches!(action,AuditAction::Attempt {run_id,assignment_id,operation:AttemptOperation::Control {action:codex_app_server_protocol::AuditAttemptAction::Retry,action_key:Some(key)}} if run_id=="run-1" && assignment_id=="assignment-original" && key=="same-action")
    );
    for operation in ["read", "observe", "wait", "cancel", "ignore", "retry"] {
        assert!(parse_command(&format!("attempt {operation} run-1 assignment-original")).is_ok());
    }
}

#[test]
fn attempt_actions_distinguish_wanted_failure_uncertainty_exclusion_and_readiness() {
    use codex_app_server_protocol::AuditAttemptAction::{Cancel, Ignore, Retry};
    let mut target = attempt_fixture();
    assert_eq!(attempt_actions(&target), vec![Cancel, Ignore, Retry]);
    target.operation_state = "failed".into();
    assert_eq!(attempt_actions(&target), vec![Ignore, Retry]);
    target.operation_state = "unknown".into();
    assert_eq!(attempt_actions(&target), vec![Cancel, Ignore, Retry]);
    target.disposition = "cancelled".into();
    assert_eq!(attempt_actions(&target), vec![Retry]);
    target.retry_eligible = false;
    assert!(attempt_actions(&target).is_empty());
    target.disposition = "ignored".into();
    target.retry_eligible = true;
    assert!(attempt_actions(&target).is_empty());
    target.disposition = "active".into();
    target.current = false;
    assert!(attempt_actions(&target).is_empty());
    target.current = true;
    target.completed = true;
    assert!(attempt_actions(&target).is_empty());
}

#[test]
fn unpublished_candidate_material_never_enters_conversation_context() {
    let mut view:AuditRunView=serde_json::from_value(json!({"runId":"run","snapshotId":"snapshot","assignmentId":"assignment","phase":"reporting","operationState":"committed",
        "observationPending":false,"capacityReleased":false,"revision":3,"cancellationRequested":false,"outcome":null,"rawResult":"UNVALIDATED_JSON","report":"UNPUBLISHED_CANDIDATE"})).unwrap();
    let thread = ThreadId::new();
    let mut contexts = ReportContexts::default();
    contexts.stage(thread, &view);
    assert!(contexts.for_turn(thread).is_none());
    assert!(report_context(&view).is_none());
    view.phase = AuditRunPhase::CompletedWithFailures;
    view.report = Some("AUTHORITATIVE_PUBLISHED_REPORT".into());
    contexts.stage(thread, &view);
    let context = contexts.for_turn(thread).unwrap();
    let value = &context.values().next().unwrap().value;
    assert!(value.contains("AUTHORITATIVE_PUBLISHED_REPORT"));
    assert!(!value.contains("UNVALIDATED_JSON"));
    assert!(!value.contains("UNPUBLISHED_CANDIDATE"));
}

#[tokio::test]
async fn shared_reviewer_confirmation_sends_only_one_original_authorised_target() {
    use codex_app_server_client::{AppServerClient, CloudClient, CloudOperation};
    use codex_app_server_protocol::{
        AUDIT_PROTOCOL_VERSION, AuditAttemptAction, AuditAttemptControlResponse, JSONRPCErrorError,
    };
    for action in [
        AuditAttemptAction::Cancel,
        AuditAttemptAction::Ignore,
        AuditAttemptAction::Retry,
    ] {
        for decision_kind in ["decline", "dismiss", "accept", "stale"] {
            let (cloud, mut commands, _) = CloudClient::channel(8);
            let client = AppServerClient::Cloud(cloud);
            let audits = AuditClient::new(client.request_handle());
            let (tx, mut events) = tokio::sync::mpsc::unbounded_channel();
            let sender = AppEventSender::new(tx);
            let target = attempt_fixture();
            let worker_action = action.clone();
            let thread = ThreadId::new();
            let task = tokio::spawn(async move {
                confirm_and_control_attempt(
                    &audits,
                    thread,
                    target,
                    worker_action,
                    Some("durable-action-key".into()),
                    &sender,
                )
                .await
            });
            let capability = commands.recv().await.unwrap();
            assert!(
                matches!(&capability.operation,CloudOperation::Request(r) if matches!(r.as_ref(),ClientRequest::AuditCapabilities {..}))
            );
            capability
                .response
                .send(Ok(Ok(json!({"protocolVersion":AUDIT_PROTOCOL_VERSION}))))
                .unwrap();
            let AppEvent::AuditAttemptConfirm {
                confirmation,
                decision,
                ..
            } = events.recv().await.unwrap()
            else {
                panic!("confirmation")
            };
            assert_eq!(confirmation.target.assignment_id, "assignment-original");
            assert_eq!(confirmation.request.action_key, "durable-action-key");
            let disclosure = confirmation.disclosure();
            assert!(disclosure.contains("native-model"));
            assert!(disclosure.contains("assignment-original"));
            assert!(
                commands.try_recv().is_err(),
                "no mutation while confirmation is open"
            );
            match decision_kind {
                "decline" => {
                    decision.send(false).unwrap();
                }
                "dismiss" => drop(decision),
                _ => decision.send(true).unwrap(),
            }
            if matches!(decision_kind, "accept" | "stale") {
                let command = commands.recv().await.unwrap();
                let CloudOperation::Request(request) = &command.operation else {
                    panic!("request")
                };
                let ClientRequest::AuditAttemptControl { params, .. } = request.as_ref() else {
                    panic!("exact control")
                };
                assert_eq!(params.run_id, "run-1");
                assert_eq!(params.assignment_id, "assignment-original");
                assert_eq!(params.action, action);
                assert_eq!(params.action_key, "durable-action-key");
                assert_eq!(params.authorise_retry, action == AuditAttemptAction::Retry);
                assert_eq!(
                    params.acknowledge_possible_overlap,
                    action == AuditAttemptAction::Retry
                );
                if decision_kind == "stale" {
                    command
                        .response
                        .send(Ok(Err(JSONRPCErrorError {
                            code: -32602,
                            message: "collection ready or exact predecessor superseded".into(),
                            data: None,
                        })))
                        .unwrap();
                } else {
                    command
                        .response
                        .send(Ok(Ok(serde_json::to_value(AuditAttemptControlResponse {
                            action_key: "durable-action-key".into(),
                            assignment_id: "assignment-original".into(),
                            disposition: "cancelled".into(),
                            cutoff: Some(3),
                            replacement_assignment_id: None,
                            replacement_operation_id: None,
                            completed_won: false,
                            possible_overlap: true,
                        })
                        .unwrap())))
                        .unwrap();
                }
            }
            assert_eq!(task.await.unwrap().is_ok(), decision_kind == "accept");
            assert!(
                commands.try_recv().is_err(),
                "no successor lookup, replay or second control"
            );
        }
    }
}

#[tokio::test]
async fn whole_audit_abort_confirms_once_and_has_no_reporting_phase_gate() {
    use codex_app_server_client::{AppServerClient, CloudClient, CloudOperation};
    for accepted in [None, Some(false), Some(true)] {
        let (cloud, mut commands, _) = CloudClient::channel(8);
        let client = AppServerClient::Cloud(cloud);
        let (events, mut receive) = {
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            (AppEventSender::new(tx), rx)
        };
        let task = tokio::spawn(execute(
            client.request_handle(),
            ThreadId::new(),
            AuditAction::Abort("reporting-run".into()),
            events,
        ));
        let capability = commands.recv().await.unwrap();
        capability
            .response
            .send(Ok(Ok(
                json!({"protocolVersion":codex_app_server_protocol::AUDIT_PROTOCOL_VERSION}),
            )))
            .unwrap();
        let AppEvent::AuditAbortConfirm {
            run_id, decision, ..
        } = receive.recv().await.unwrap()
        else {
            panic!("abort confirmation")
        };
        assert_eq!(run_id, "reporting-run");
        assert!(
            commands.try_recv().is_err(),
            "no read/freeze gate or mutation before authorisation"
        );
        if let Some(accepted) = accepted {
            decision.send(accepted).unwrap();
        } else {
            drop(decision);
        }
        if accepted == Some(true) {
            let command = commands.recv().await.unwrap();
            let CloudOperation::Request(request) = &command.operation else {
                panic!("request")
            };
            assert!(
                matches!(request.as_ref(),ClientRequest::AuditCancel {params,..} if params.run_id=="reporting-run")
            );
            command.response.send(Ok(Ok(json!({"run":{"runId":"reporting-run","snapshotId":"snapshot","assignmentId":"assignment","phase":"reporting","operationState":"unknown","observationPending":true,"capacityReleased":true,"revision":5,"cancellationRequested":true,"outcome":null,"rawResult":null,"report":null}})))).unwrap();
        }
        let result = task.await.unwrap();
        assert_eq!(result.is_ok(), accepted == Some(true));
        if let Ok(run) = result {
            assert_eq!(run["phase"], "reporting");
        }
        assert!(
            commands.try_recv().is_err(),
            "one whole-audit transaction, no per-reviewer cancellation loop"
        );
    }
}

#[tokio::test]
async fn attempt_wait_observes_original_history_and_never_follows_a_successor() {
    use codex_app_server_client::{AppServerClient, CloudClient, CloudOperation};
    let (cloud, mut commands, _) = CloudClient::channel(8);
    let client = AppServerClient::Cloud(cloud);
    let (tx, mut receive) = tokio::sync::mpsc::unbounded_channel();
    let events = AppEventSender::new(tx);
    let wake = std::sync::Arc::new(tokio::sync::Notify::new());
    let task = tokio::spawn(observe_attempt(
        client.request_handle(),
        ThreadId::new(),
        attempt_fixture(),
        events,
        wake.clone(),
    ));
    assert!(matches!(
        receive.recv().await.unwrap(),
        AppEvent::AuditAttemptObserved { .. }
    ));
    wake.notify_one();
    let command = commands.recv().await.unwrap();
    let CloudOperation::Request(request) = &command.operation else {
        panic!("request")
    };
    assert!(
        matches!(request.as_ref(),ClientRequest::AuditAttemptRead {params,..} if params.assignment_id=="assignment-original")
    );
    let mut history = attempt_fixture();
    history.current = false;
    history.disposition = "retried".into();
    command
        .response
        .send(Ok(Ok(
            json!({"attempt":history,"entries":[],"nextSequence":3,"rawResult":null}),
        )))
        .unwrap();
    let AppEvent::AuditAttemptObserved { attempt, .. } = receive.recv().await.unwrap() else {
        panic!("history update")
    };
    assert_eq!(attempt.assignment_id, "assignment-original");
    assert!(!attempt.current);
    task.await.unwrap();
    assert!(
        commands.try_recv().is_err(),
        "no successor read, inference or control"
    );
}
