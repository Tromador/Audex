use super::*;
use codex_app_server_client::{AppServerClient, CloudClient, CloudOperation};

fn fixture_run() -> AuditRun {
    serde_json::from_value(json!({"runId":"run","targets":["source"],"createdAt":"time","reviewers":[],"complete":false,"summary":null,"sourceNotice":"Source may have changed"})).unwrap()
}
#[test]
fn command_paths_snippet_selections_presets_and_removed_tools() {
    for command in [
        "one",
        "one two",
        "one/a.rs one/b.rs",
        "--snippet 'text with spaces' --name example.rs",
    ] {
        assert!(matches!(
            parse_command(command).unwrap(),
            AuditAction::Start(_)
        ));
    }
    let AuditAction::Start(args)=parse_command("'named path/a.rs' --reviewer native:model:high --reviewer claude_subscription:opus:low --guidance AUDEX.md --brief 'specific brief'").unwrap() else {panic!()};
    let selected = selections(&args, &BTreeMap::new()).unwrap();
    assert_eq!(selected[0].model, "model");
    assert_eq!(selected[0].effort.as_deref(), Some("high"));
    assert_eq!(selected[1].effort.as_deref(), Some("low"));
    assert_eq!(args.guidance.as_deref(), Some("AUDEX.md"));
    let presets = BTreeMap::from([
        ("default".into(), vec!["native:default:medium".into()]),
        (
            "pair".into(),
            vec!["native:chosen".into(), "claude_subscription:chosen".into()],
        ),
    ]);
    let AuditAction::Start(default) = parse_command("source").unwrap() else {
        panic!()
    };
    assert_eq!(selections(&default, &presets).unwrap()[0].model, "default");
    let AuditAction::Start(pair) = parse_command("source --with pair").unwrap() else {
        panic!()
    };
    assert_eq!(selections(&pair, &presets).unwrap().len(), 2);
    assert_eq!(
        selections(&args, &presets).unwrap().len(),
        2,
        "default does not silently add reviewers"
    );
    for command in [
        "source --sequential",
        "source --agents-md AGENTS.md",
        "source --preflight",
        "retry run",
        "cancel",
    ] {
        assert!(parse_command(command).is_err());
    }
    assert_eq!(TOOLS.len(), 4);
    assert!(tool_action("audit_source", json!({"runId":"run"})).is_err());
    for view in ["reviewers", "findings", "finding", "output", "diagnostics"] {
        assert!(matches!(
            tool_action(
                "audit_read",
                json!({"runId":"run","view":view,"offset":1,"limit":2})
            )
            .unwrap(),
            AuditAction::Read(_)
        ));
    }
}

#[tokio::test]
async fn exactly_one_confirmation_and_decline_sends_no_start() {
    for accept in [false, true] {
        let (client, mut commands, _incoming) = CloudClient::channel(8);
        let client = AppServerClient::Cloud(client);
        let handle = client.request_handle();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let events = AppEventSender::new(tx);
        let action =
            parse_command("'target as given' --reviewer native:chosen:high --guidance AUDEX.md")
                .unwrap();
        let thread = ThreadId::new();
        let work = execute(handle, thread, action, events);
        let server = async {
            let command = commands.recv().await.unwrap();
            let CloudOperation::Request(request) = command.operation else {
                panic!()
            };
            assert!(matches!(*request, ClientRequest::ThreadRead { .. }));
            command.response.send(Ok(Ok(json!({"thread":{"id":thread.to_string(),"sessionId":thread.to_string(),"ephemeral":false,"preview":"","modelProvider":"mock","createdAt":0,"updatedAt":0,"status":{"type":"idle"},"path":null,"cwd":"/source","cliVersion":"0","source":"cli","turns":[]}})))).unwrap();
            let AppEvent::AuditConfirm {
                params, decision, ..
            } = rx.recv().await.unwrap()
            else {
                panic!("one disclosure")
            };
            let disclosure = codex_app_server_client::audit::start_disclosure(&params);
            assert!(disclosure.contains("target as given"));
            assert!(disclosure.contains("AUDEX.md"));
            assert!(disclosure.contains("chosen"));
            assert!(disclosure.contains("high"));
            decision.send(accept).unwrap();
            if accept {
                let command = commands.recv().await.unwrap();
                let CloudOperation::Request(request) = command.operation else {
                    panic!()
                };
                assert!(matches!(*request, ClientRequest::AuditStart { .. }));
                command
                    .response
                    .send(Ok(Ok(json!({"run":fixture_run()}))))
                    .unwrap();
                assert!(matches!(
                    rx.recv().await.unwrap(),
                    AppEvent::AuditOpened { compare: true, .. }
                ));
            }
        };
        let (result, ()) = tokio::join!(work, server);
        assert_eq!(result.is_ok(), accept);
        assert!(commands.try_recv().is_err());
        assert!(rx.try_recv().is_err());
    }
}

#[test]
fn progress_shows_one_line_per_reviewer_and_retained_attempts() {
    let mut run = fixture_run();
    for (index, state) in [
        "queued",
        "running",
        "succeeded",
        "failed",
        "cancelled",
        "interrupted",
    ]
    .into_iter()
    .enumerate()
    {
        run.reviewers.push(AuditReviewerStatus {
            reviewer: format!("reviewer-{}", index + 1),
            selection: AuditReviewer {
                route: "native".into(),
                model: "chosen".into(),
                effort: Some("high".into()),
            },
            attempt: 1,
            trigger: "initial".into(),
            state: state.into(),
            reason: (state == "failed").then(|| "failure reason".into()),
            tool_calls: 7,
            finding_count: (state == "succeeded").then_some(3),
            has_output: matches!(state, "failed" | "cancelled"),
        });
    }
    let shown = progress(&run);
    assert_eq!(shown.lines().count(), 7);
    for detail in [
        "queued",
        "running — 7 tool calls",
        "succeeded — 3 findings",
        "failed — failure reason",
        "cancelled (partial output retained)",
        "interrupted",
    ] {
        assert!(shown.contains(detail), "{shown}");
    }
    let mut replacement = run.reviewers[3].clone();
    replacement.attempt = 2;
    replacement.trigger = "automatic_replacement".into();
    replacement.state = "running".into();
    run.reviewers.push(replacement.clone());
    let shown = progress(&run);
    assert_eq!(shown.lines().count(), 7);
    assert!(shown.contains("attempt 2 (automatic replacement)"));
    assert!(shown.contains("earlier: attempt 1 failed"));
    replacement.attempt = 3;
    replacement.trigger = "operator_retry".into();
    replacement.state = "queued".into();
    run.reviewers.push(replacement);
    let shown = progress(&run);
    assert_eq!(shown.lines().count(), 7);
    assert!(shown.contains("attempt 3 (operator retry)"));
    assert!(shown.contains("attempt 2 running"));
}
