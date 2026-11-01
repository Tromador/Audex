//! S4's real IPC journal reader and shared controls, with gated local inference only.
use super::*;
use crate::audit::activity::ActivityViewer;
use codex_app_server_protocol::{AuditReviewerSelection, AuditScheduling};
use std::sync::atomic::{AtomicUsize, Ordering};

#[test]
fn activity_reader_controls_disconnect_and_slow_viewer_over_daemon_ipc() -> anyhow::Result<()> {
    core_test_support::test_codex::run_test_with_large_stack("s4-activity-daemon", || async {
        scenario()
            .await
            .map_err(|error| anyhow::anyhow!("{error:#}"))
    })
}
async fn apply_next(
    view: &mut ActivityViewer,
    events: &mut mpsc::UnboundedReceiver<AppEvent>,
) -> Result<()> {
    loop {
        let event = tokio::time::timeout(Duration::from_secs(10), events.recv())
            .await?
            .unwrap();
        if let AppEvent::AuditActivityPage {
            viewer_id,
            page,
            consumed,
            ..
        } = event
        {
            assert_eq!(viewer_id, view.id);
            view.apply(*page);
            consumed.send(()).unwrap();
            return Ok(());
        }
    }
}
async fn scenario() -> Result<()> {
    let parent = std::env::var_os("AUDEX_TEST_TMPDIR").expect("explicit fixture parent");
    let daemon = support::TestDaemon::new_in(std::path::Path::new(&parent))
        .map_err(|error| color_eyre::eyre::eyre!("{error:#}"))?;
    let fixture_home = daemon.home.path().to_owned();
    let release = Arc::new(tokio::sync::Notify::new());
    let requests = Arc::new(AtomicUsize::new(0));
    let gate = release.clone();
    let invocations = requests.clone();
    let router=axum::Router::new().route("/v1/responses",axum::routing::post(move |axum::Json(body):axum::Json<Value>| {
        let gate=gate.clone();let invocations=invocations.clone();
        async move {
            use futures::StreamExt;
            let index=invocations.fetch_add(1,Ordering::SeqCst);
            let assignment=body["input"].as_array().unwrap().iter().filter_map(|item|item["content"].as_array()).flatten().filter_map(|part|part["text"].as_str()).filter_map(|text|serde_json::from_str::<Value>(text).ok()).find(|value|value.get("assignment_id").is_some()).unwrap();
            let prefix=responses::sse(vec![responses::ev_response_created(&format!("generation-{index}")),responses::ev_message_item_added("partial",""),responses::ev_output_text_delta("COMMITTED_PUBLIC_PREFIX")]);
            let result=json!({"schema_version":1,"snapshot_id":assignment["snapshot_id"],"assignment_id":assignment["assignment_id"],"summary":"VALID_COMPLETED_REVIEW","findings":[]});
            let final_events=responses::sse(vec![responses::ev_output_text_delta("LATER_PROVIDER_TEXT"),responses::ev_assistant_message("final-result",&result.to_string()),responses::ev_completed(&format!("generation-{index}"))]);
            let stream=futures::stream::iter([Ok::<_,std::convert::Infallible>(axum::body::Bytes::from(prefix))]).chain(futures::stream::once(async move {gate.notified().await;Ok(axum::body::Bytes::from(final_events))}));
            ([("content-type","text/event-stream"),("OpenAI-Model","audit-test")],axum::body::Body::from_stream(stream))
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let provider_url = format!("http://{}", listener.local_addr()?);
    let provider = tokio::spawn(async move { axum::serve(listener, router).await });
    std::fs::write(
        fixture_home.join("config.toml"),
        format!(
            r#"
model="audit-test"
model_provider="activity_fixture"
[features]
plugins=false
remote_plugin=false
plugin_discovery_prefetch=false
[model_providers.activity_fixture]
name="S4 loopback activity fixture"
base_url="{provider_url}/v1"
wire_api="responses"
request_max_retries=0
stream_max_retries=0
"#
        ),
    )?;
    std::fs::write(
        fixture_home.join("app-server-daemon/settings.json"),
        r#"{"shutdownGraceSeconds":0,"updater":{"autoUpdateEnabled":false}}"#,
    )?;
    daemon
        .lifecycle("start")
        .map_err(|error| color_eyre::eyre::eyre!("{error:#}"))?;
    let config = ConfigBuilder::default()
        .codex_home(fixture_home.clone())
        .harness_overrides(ConfigOverrides {
            cwd: Some(fixture_home.clone()),
            ..Default::default()
        })
        .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
        .build()
        .await?;
    let mut session = connect(&fixture_home).await?;
    let thread = session.start_thread(&config).await?.session.thread_id;
    let audits = AuditClient::new(session.request_handle());
    let run = audits
        .start(AuditStartParams {
            request_key: "s4-owned-activity".into(),
            source: AuditSource::Snippet {
                name: "source.rs".into(),
                content: "fn unchanged() {}".into(),
            },
            brief: "Review this sealed source".into(),
            provider: None,
            model: None,
            reasoning_effort: None,
            expected_served_model: None,
            reviewers: Some(
                (0..3)
                    .map(|_| AuditReviewerSelection {
                        provider: "activity_fixture".into(),
                        model: "audit-test".into(),
                        expected_served_model: None,
                        reasoning_effort: None,
                        execution_adapter: Default::default(),
                    })
                    .collect(),
            ),
            scheduling: AuditScheduling::Automatic,
            audex_guidance: None,
            agents_evidence: None,
        })
        .await?;
    let targets = wait_attempts(&audits, &run.run_id, |items| {
        items.len() == 3 && items.iter().all(|a| a.output_sequence > 0)
    })
    .await?;
    assert_eq!(requests.load(Ordering::SeqCst), 3);
    let original = targets[0].clone();
    let (events, mut receive) = sender();
    let mut view = ActivityViewer::new(
        thread,
        original.clone(),
        &crate::keymap::RuntimeKeymap::defaults(),
    );
    view.reconnect(session.request_handle(), events.clone());
    apply_next(&mut view, &mut receive).await?;
    assert!(view.cursor > 0);
    let prefix_cursor = view.cursor;
    assert_eq!(
        requests.load(Ordering::SeqCst),
        3,
        "opening/reading does not infer"
    );
    // Real exact-target direct Retry through the SAME S3 path used by contextual R.
    let (control_events, mut controls) = sender();
    let handle = session.request_handle();
    let target = original.clone();
    let retry = tokio::spawn(async move {
        confirm_and_control_attempt(
            &AuditClient::new(handle),
            thread,
            target,
            AuditAttemptAction::Retry,
            Some("s4-direct-retry".into()),
            &control_events,
        )
        .await
    });
    let AppEvent::AuditAttemptConfirm {
        confirmation,
        decision,
        ..
    } = controls.recv().await.unwrap()
    else {
        panic!("shared confirmation")
    };
    assert_eq!(confirmation.request.assignment_id, original.assignment_id);
    assert!(confirmation.disclosure().contains("additional charges"));
    assert_eq!(audits.attempts(run.run_id.clone()).await?.attempts.len(), 3);
    decision.send(true).unwrap();
    let retried = retry.await?.map_err(color_eyre::eyre::Report::msg)?;
    let replacement = retried.replacement_assignment_id.clone().unwrap();
    let after_retry = wait_attempts(&audits, &run.run_id, |items| {
        items.len() == 4
            && items
                .iter()
                .any(|a| a.assignment_id == replacement && a.output_sequence > 0)
    })
    .await?;
    assert_eq!(requests.load(Ordering::SeqCst), 4);
    view.wake.notify_one();
    apply_next(&mut view, &mut receive).await?;
    assert_eq!(view.target.assignment_id, original.assignment_id);
    assert!(!view.target.current);
    assert!(
        view.action_for_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('R'),
            crossterm::event::KeyModifiers::NONE
        ))
        .is_none()
    );
    // A stale confirmation remains bound; server refusal causes no assignment or inference.
    let (control_events, mut controls) = sender();
    let target = original.clone();
    let handle = session.request_handle();
    let stale = tokio::spawn(async move {
        confirm_and_control_attempt(
            &AuditClient::new(handle),
            thread,
            target,
            AuditAttemptAction::Retry,
            Some("s4-stale-retry".into()),
            &control_events,
        )
        .await
    });
    let AppEvent::AuditAttemptConfirm { decision, .. } = controls.recv().await.unwrap() else {
        panic!("shared confirmation")
    };
    decision.send(true).unwrap();
    assert!(stale.await?.is_err());
    assert_eq!(audits.attempts(run.run_id.clone()).await?.attempts.len(), 4);
    assert_eq!(requests.load(Ordering::SeqCst), 4);
    let ignored_target = after_retry
        .iter()
        .find(|a| a.assignment_id == targets[1].assignment_id)
        .unwrap()
        .clone();
    control(
        AuditClient::new(session.request_handle()),
        thread,
        ignored_target.clone(),
        AuditAttemptAction::Ignore,
        "s4-ignore",
    )
    .await?;
    let ignored = audits
        .read_attempt(codex_app_server_protocol::AuditAttemptReadParams {
            run_id: run.run_id.clone(),
            assignment_id: ignored_target.assignment_id.clone(),
            after_sequence: Some(0),
            limit: Some(128),
        })
        .await?;
    assert_eq!(ignored.attempt.disposition, "ignored");
    assert!(!ignored.attempt.retry_eligible);
    let mut ignored_view = ActivityViewer::new(
        thread,
        ignored.attempt.clone(),
        &crate::keymap::RuntimeKeymap::defaults(),
    );
    ignored_view.apply(Ok(ignored));
    assert!(
        ignored_view
            .action_for_key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('R'),
                crossterm::event::KeyModifiers::NONE
            ))
            .is_none()
    );
    drop(ignored_view);
    let replacement_target = after_retry
        .iter()
        .find(|a| a.assignment_id == replacement)
        .unwrap()
        .clone();
    control(
        AuditClient::new(session.request_handle()),
        thread,
        replacement_target,
        AuditAttemptAction::Cancel,
        "s4-cancel-replacement",
    )
    .await?;
    // Closing a viewer and frontend disconnect leave all executions owned by the daemon.
    drop(view);
    drop(audits);
    session.shutdown().await?;
    let reconnected = connect(&fixture_home).await?;
    let audits = AuditClient::new(reconnected.request_handle());
    let historical = audits
        .read_attempt(codex_app_server_protocol::AuditAttemptReadParams {
            run_id: run.run_id.clone(),
            assignment_id: original.assignment_id.clone(),
            after_sequence: Some(0),
            limit: Some(128),
        })
        .await?;
    let cancelled = audits
        .read_attempt(codex_app_server_protocol::AuditAttemptReadParams {
            run_id: run.run_id.clone(),
            assignment_id: replacement.clone(),
            after_sequence: Some(0),
            limit: Some(128),
        })
        .await?;
    assert_eq!(cancelled.attempt.disposition, "cancelled");
    assert!(cancelled.attempt.retry_eligible);
    let (pending_events, mut pending_receive) = sender();
    let handle = reconnected.request_handle();
    let pending_target = cancelled.attempt;
    let pending_retry = tokio::spawn(async move {
        confirm_and_control_attempt(
            &AuditClient::new(handle),
            thread,
            pending_target,
            AuditAttemptAction::Retry,
            Some("s4-ready-stale".into()),
            &pending_events,
        )
        .await
    });
    let AppEvent::AuditAttemptConfirm {
        confirmation,
        decision: pending_decision,
        ..
    } = pending_receive.recv().await.unwrap()
    else {
        panic!("shared readiness confirmation")
    };
    assert_eq!(confirmation.target.assignment_id, replacement);
    let cutoff = historical.attempt.output_cutoff.unwrap();
    assert!(historical.entries.iter().all(|e| e.sequence <= cutoff));
    let (slow_events, mut slow_receive) = sender();
    let mut slow = ActivityViewer::new(
        thread,
        historical.attempt.clone(),
        &crate::keymap::RuntimeKeymap::defaults(),
    );
    slow.reconnect(reconnected.request_handle(), slow_events);
    let held = slow_receive.recv().await.unwrap(); // deliberately do NOT acknowledge this page
    assert!(matches!(&held, AppEvent::AuditActivityPage { .. }));
    release.notify_waiters();
    let published = published(&audits, &run.run_id).await?;
    assert_eq!(
        requests.load(Ordering::SeqCst),
        4,
        "reconnect/slow viewer causes no inference replay"
    );
    assert!(terminal(&published));
    pending_decision.send(true).unwrap();
    assert!(
        pending_retry.await?.is_err(),
        "readiness refuses the original Cancelled target"
    );
    assert_eq!(audits.attempts(run.run_id.clone()).await?.attempts.len(), 4);
    assert!(
        published
            .report
            .as_ref()
            .unwrap()
            .contains("VALID_COMPLETED_REVIEW")
    );
    let retained = audits
        .read_attempt(codex_app_server_protocol::AuditAttemptReadParams {
            run_id: run.run_id.clone(),
            assignment_id: original.assignment_id.clone(),
            after_sequence: Some(0),
            limit: Some(128),
        })
        .await?;
    assert_eq!(retained.attempt.output_cutoff, Some(cutoff));
    assert!(
        retained
            .entries
            .iter()
            .all(|e| e.sequence <= cutoff && !e.payload.contains("LATER_PROVIDER_TEXT"))
    );
    assert_eq!(requests.load(Ordering::SeqCst), 4);
    if let Some(root) = std::env::var_os("AUDEX_S4_EVIDENCE") {
        std::fs::write(
            std::path::PathBuf::from(root).join("activity-ipc.json"),
            serde_json::to_vec_pretty(
                &json!({"runId":run.run_id,"originalAssignment":original.assignment_id,"replacementAssignment":replacement,"ignoredAssignment":ignored_target.assignment_id,"initialCursor":prefix_cursor,"durableCutoff":cutoff,"publishedReport":published.report,"providerRequests":4,"phase":published.phase,"slowPageUnacknowledgedAtPublication":true,"readinessRetryRefused":true}),
            )?,
        )?;
    }
    drop(held);
    drop(slow);
    reconnected.shutdown().await?;
    provider.abort();
    Ok(())
}
