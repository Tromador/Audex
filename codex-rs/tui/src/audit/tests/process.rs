//! S3 main-TUI request/confirmation paths over disposable daemon IPC. Loopback only.
use super::execution::Result;
use super::*;
use crate::app_server_session::{AppServerSession, ThreadParamsMode};
use crate::legacy_core::config::{ConfigBuilder, ConfigOverrides};
use codex_app_server_protocol::{AuditAttemptAction, AuditAttemptView};
use codex_config::LoaderOverrides;
use core_test_support::{
    responses,
    streaming_sse::{StreamingSseChunk, start_streaming_sse_server},
};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};
use wiremock::{
    Mock, ResponseTemplate,
    matchers::{method, path},
};

#[path = "../../../../cli/tests/app_server_daemon/support.rs"]
mod support;

fn sender() -> (AppEventSender, mpsc::UnboundedReceiver<AppEvent>) {
    let (tx, rx) = mpsc::unbounded_channel();
    (AppEventSender::new(tx), rx)
}
async fn connect(home: &std::path::Path) -> Result<AppServerSession> {
    let advertised = codex_app_server_client::app_server_control_socket_path(home)?;
    let socket = codex_app_server_client::verified_local_daemon_socket(advertised.as_path())?
        .ok_or_else(|| color_eyre::eyre::eyre!("fixture socket missing"))?;
    let client = crate::connect_remote_app_server(
        codex_app_server_client::RemoteAppServerEndpoint::UnixSocket {
            socket_path: socket,
        },
    )
    .await?;
    Ok(AppServerSession::new(
        crate::backend_id::BackendId::new(),
        client,
        ThreadParamsMode::Remote,
    ))
}
async fn start_confirmed(
    session: &AppServerSession,
    thread: ThreadId,
    args: StartArguments,
    decision: Option<bool>,
) -> Result<Option<AuditRunView>> {
    let (events, mut receive) = sender();
    let task = tokio::spawn(execute(
        session.request_handle(),
        thread,
        AuditAction::Start(args),
        events,
    ));
    let AppEvent::AuditConfirm {
        params,
        decision: answer,
        ..
    } = receive.recv().await.unwrap()
    else {
        panic!("start confirmation")
    };
    assert_eq!(params.reviewers.as_ref().unwrap().len(), 3);
    let audits = AuditClient::new(session.request_handle());
    let before = audits
        .list(AuditListParams {
            after: None,
            limit: None,
        })
        .await?
        .runs
        .len();
    if let Some(decision) = decision {
        answer.send(decision).unwrap();
    } else {
        drop(answer);
    }
    let result = task.await?;
    if decision == Some(true) {
        Ok(Some(serde_json::from_value(
            result.map_err(color_eyre::eyre::Report::msg)?,
        )?))
    } else {
        assert!(result.is_err());
        assert_eq!(
            audits
                .list(AuditListParams {
                    after: None,
                    limit: None
                })
                .await?
                .runs
                .len(),
            before
        );
        Ok(None)
    }
}
async fn control(
    audits: AuditClient,
    thread: ThreadId,
    target: AuditAttemptView,
    action: AuditAttemptAction,
    key: &str,
) -> Result<codex_app_server_protocol::AuditAttemptControlResponse> {
    let (events, mut receive) = sender();
    let key = key.to_owned();
    let task = tokio::spawn(async move {
        confirm_and_control_attempt(&audits, thread, target, action, Some(key), &events).await
    });
    let AppEvent::AuditAttemptConfirm { decision, .. } = receive.recv().await.unwrap() else {
        panic!("exact attempt confirmation")
    };
    decision.send(true).unwrap();
    task.await?.map_err(color_eyre::eyre::Report::msg)
}
async fn wait_attempts(
    audits: &AuditClient,
    id: &str,
    test: impl Fn(&[AuditAttemptView]) -> bool,
) -> Result<Vec<AuditAttemptView>> {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let attempts = audits.attempts(id.to_owned()).await?.attempts;
            if test(&attempts) {
                return Ok(attempts);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?
}
async fn published(audits: &AuditClient, id: &str) -> Result<AuditRunView> {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let run = audits.read(id.to_owned()).await?;
            if terminal(&run) && run.report.is_some() {
                return Ok(run);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?
}

#[test]
fn main_tui_rosters_controls_stale_confirmations_abort_and_reconnect_over_daemon_ipc()
-> anyhow::Result<()> {
    core_test_support::test_codex::run_test_with_large_stack("s3-main-tui-daemon", || async {
        scenario()
            .await
            .map_err(|error| anyhow::anyhow!("{error:#}"))
    })
}
async fn scenario() -> Result<()> {
    let parent = std::env::var_os("AUDEX_TEST_TMPDIR").expect("explicit fixture parent");
    let daemon = support::TestDaemon::new_in(std::path::Path::new(&parent))
        .map_err(|error| color_eyre::eyre::eyre!("{error:#}"))?;
    let fixture_home = daemon.home.path().to_owned();
    let mut releases = Vec::new();
    let mut scripts = Vec::new();
    for index in 0..8 {
        let (release, gate) = oneshot::channel();
        releases.push(release);
        let mut created = responses::ev_response_created(&format!("held-{index}"));
        created["response"]["headers"] = json!({"OpenAI-Model":"audit-test"});
        scripts.push(vec![
            StreamingSseChunk {
                gate: None,
                body: responses::sse(vec![
                    created,
                    responses::ev_message_item_added("partial", ""),
                    responses::ev_output_text_delta("INCOMPLETE_NATIVE_PREFIX"),
                ]),
            },
            StreamingSseChunk {
                gate: Some(gate),
                body: responses::sse(vec![responses::ev_completed(&format!("held-{index}"))]),
            },
        ]);
    }
    let (provider, _) = start_streaming_sse_server(scripts).await;
    // Fresh response headers/prefix arrive first; the final peer completion is
    // explicitly released while Retry confirmation remains open (A22).
    let readiness_gate = Arc::new(tokio::sync::Notify::new());
    let readiness_requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let gate = readiness_gate.clone();
    let invocations = readiness_requests.clone();
    let router=axum::Router::new().route("/v1/responses",axum::routing::post(move |axum::Json(body):axum::Json<Value>| {
        let gate=gate.clone();let invocations=invocations.clone();
        async move {
            use futures::StreamExt;
            invocations.fetch_add(1,std::sync::atomic::Ordering::SeqCst);
            let assignment=body["input"].as_array().unwrap().iter().filter_map(|item|item["content"].as_array()).flatten().filter_map(|part|part["text"].as_str()).filter_map(|text|serde_json::from_str::<Value>(text).ok()).find(|value|value.get("assignment_id").is_some()).unwrap();
            let prefix=responses::sse(vec![responses::ev_response_created("readiness-peer"),responses::ev_assistant_message("peer-prefix","READY_PEER_RECEIVED")]);
            let result=json!({"schema_version":1,"snapshot_id":assignment["snapshot_id"],"assignment_id":assignment["assignment_id"],"summary":"READY_PEER_COMPLETED","findings":[]});
            let final_events=responses::sse(vec![responses::ev_assistant_message("peer-final",&result.to_string()),responses::ev_completed("readiness-peer")]);
            let stream=futures::stream::iter([Ok::<_,std::convert::Infallible>(axum::body::Bytes::from(prefix))]).chain(futures::stream::once(async move {gate.notified().await;Ok(axum::body::Bytes::from(final_events))}));
            ([("content-type","text/event-stream"),("OpenAI-Model","audit-test")],axum::body::Body::from_stream(stream))
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let readiness_url = format!("http://{}", listener.local_addr()?);
    let readiness_server = tokio::spawn(async move { axum::serve(listener, router).await });
    let completed = responses::start_mock_server().await;
    Mock::given(method("POST")).and(path("/v1/responses")).respond_with(|request:&wiremock::Request| {
        let body:Value=request.body_json().unwrap();let assignment=body["input"].as_array().unwrap().iter().filter_map(|item|item["content"].as_array()).flatten()
            .filter_map(|part|part["text"].as_str()).filter_map(|text|serde_json::from_str::<Value>(text).ok()).find(|value|value.get("assignment_id").is_some()).unwrap();
        let result=json!({"schema_version":1,"snapshot_id":assignment["snapshot_id"],"assignment_id":assignment["assignment_id"],"summary":"COMPLETED_REVIEW_PEER","findings":[]});
        ResponseTemplate::new(200).insert_header("content-type","text/event-stream").insert_header("OpenAI-Model","audit-test").set_body_string(responses::sse(vec![responses::ev_response_created("complete"),responses::ev_assistant_message("result",&result.to_string()),responses::ev_completed("complete")]))
    }).mount(&completed).await;
    std::fs::write(
        fixture_home.join("config.toml"),
        format!(
            r#"
model="audit-test"
model_provider="audit_fixture"
[features]
plugins=false
remote_plugin=false
plugin_discovery_prefetch=false
[model_providers.audit_fixture]
name="Owned incomplete native fixture"
base_url="{}/v1"
wire_api="responses"
request_max_retries=0
stream_max_retries=0
[model_providers.completed_fixture]
name="Owned completed native fixture"
base_url="{}/v1"
wire_api="responses"
request_max_retries=0
stream_max_retries=0
[model_providers.readiness_fixture]
name="Owned gated readiness fixture"
base_url="{}/v1"
wire_api="responses"
request_max_retries=0
stream_max_retries=0
"#,
            provider.uri(),
            completed.uri(),
            readiness_url
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
    let args = || StartArguments {
        source: AuditSource::Snippet {
            name: "example.rs".into(),
            content: "fn main() {}\n".into(),
        },
        model: None,
        provider: None,
        reviewers: Some(
            (0..3)
                .map(|index| codex_app_server_protocol::AuditReviewerSelection {
                    provider: if index == 2 {
                        "readiness_fixture".into()
                    } else {
                        "audit_fixture".into()
                    },
                    model: "audit-test".into(),
                    reasoning_effort: None,
                    expected_served_model: None,
                    execution_adapter: Default::default(),
                })
                .collect(),
        ),
        scheduling: Default::default(),
        expected_served_model: None,
        request_key: None,
        brief: default_brief(),
        audex_guidance: Some("SHARED_EXPLICIT_GUIDANCE".into()),
        agents_evidence: Some("SHARED_EXPLICIT_EVIDENCE".into()),
    };
    start_confirmed(&session, thread, args(), Some(false)).await?;
    start_confirmed(&session, thread, args(), None).await?;
    assert!(provider.requests().await.is_empty());
    let initial = start_confirmed(&session, thread, args(), Some(true))
        .await?
        .unwrap();
    let id = initial.run_id.clone();
    let audits = AuditClient::new(session.request_handle());
    let original = wait_attempts(&audits, &id, |attempts| {
        attempts.len() == 3 && attempts.iter().all(|a| a.output_sequence > 0)
    })
    .await?;
    assert_eq!(provider.requests().await.len(), 2);
    assert_eq!(
        original
            .iter()
            .map(|a| &a.reviewer_id)
            .collect::<std::collections::HashSet<_>>()
            .len(),
        3
    );
    let (events, mut receive) = sender();
    execute(
        session.request_handle(),
        thread,
        AuditAction::Attempts(id.clone()),
        events,
    )
    .await
    .map_err(color_eyre::eyre::Report::msg)?;
    let AppEvent::AuditAttemptsLoaded { attempts, .. } = receive.recv().await.unwrap() else {
        panic!("reviewer picker")
    };
    assert_eq!(attempts, original);
    let (events, mut receive) = sender();
    let wake = Arc::new(tokio::sync::Notify::new());
    let observer = tokio::spawn(observe_attempt(
        session.request_handle(),
        thread,
        original[0].clone(),
        events,
        wake.clone(),
    ));
    assert!(matches!(
        receive.recv().await.unwrap(),
        AppEvent::AuditAttemptObserved { .. }
    ));
    session.shutdown().await?;
    wake.notify_one();
    tokio::time::timeout(Duration::from_secs(5), observer).await??;
    let session = connect(&fixture_home).await?;
    let audits = AuditClient::new(session.request_handle());
    assert!(!audits.read(id.clone()).await?.cancellation_requested);
    assert_eq!(
        audits
            .attempts(id.clone())
            .await?
            .attempts
            .iter()
            .map(|a| &a.assignment_id)
            .collect::<Vec<_>>(),
        original
            .iter()
            .map(|a| &a.assignment_id)
            .collect::<Vec<_>>()
    );
    assert_eq!(provider.requests().await.len(), 2);
    // Open one authorisation, replace that exact predecessor elsewhere, then accept the stale one.
    let (events, mut receive) = sender();
    let copy = audits.clone();
    let target = original[0].clone();
    let stale = tokio::spawn(async move {
        confirm_and_control_attempt(
            &copy,
            thread,
            target,
            AuditAttemptAction::Retry,
            Some("stale-predecessor".into()),
            &events,
        )
        .await
    });
    let AppEvent::AuditAttemptConfirm {
        decision: stale_decision,
        ..
    } = receive.recv().await.unwrap()
    else {
        panic!("stale confirmation")
    };
    assert_eq!(provider.requests().await.len(), 2);
    let retried = control(
        audits.clone(),
        thread,
        original[0].clone(),
        AuditAttemptAction::Retry,
        "authorised-retry",
    )
    .await?;
    let replacement = retried.replacement_assignment_id.unwrap();
    wait_attempts(&audits, &id, |rows| {
        rows.iter()
            .any(|a| a.assignment_id == replacement && a.output_sequence > 0)
    })
    .await?;
    stale_decision.send(true).unwrap();
    assert!(stale.await?.is_err());
    let replay = control(
        audits.clone(),
        thread,
        original[0].clone(),
        AuditAttemptAction::Retry,
        "authorised-retry",
    )
    .await?;
    assert_eq!(
        replay.replacement_assignment_id.as_deref(),
        Some(replacement.as_str())
    );
    assert_eq!(provider.requests().await.len(), 3);
    control(
        audits.clone(),
        thread,
        original[1].clone(),
        AuditAttemptAction::Cancel,
        "cancel-b",
    )
    .await?;
    let rows = audits.attempts(id.clone()).await?.attempts;
    let cancelled = rows
        .iter()
        .find(|a| a.assignment_id == original[1].assignment_id)
        .unwrap()
        .clone();
    assert!(cancelled.retry_eligible);
    let (events, mut receive) = sender();
    let copy = audits.clone();
    let stale = tokio::spawn(async move {
        confirm_and_control_attempt(
            &copy,
            thread,
            cancelled,
            AuditAttemptAction::Retry,
            Some("stale-readiness".into()),
            &events,
        )
        .await
    });
    let AppEvent::AuditAttemptConfirm { decision, .. } = receive.recv().await.unwrap() else {
        panic!("readiness confirmation")
    };
    let replacement_target = rows
        .iter()
        .find(|a| a.assignment_id == replacement)
        .unwrap()
        .clone();
    control(
        audits.clone(),
        thread,
        replacement_target,
        AuditAttemptAction::Ignore,
        "ignore-a",
    )
    .await?;
    readiness_gate.notify_one();
    assert_eq!(
        readiness_requests.load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    let ready = published(&audits, &id).await?;
    assert!(ready.collection.ready);
    let stable_report = ready.report.clone();
    decision.send(true).unwrap();
    assert!(stale.await?.is_err());
    assert_eq!(audits.attempts(id.clone()).await?.attempts.len(), 4);
    assert_eq!(audits.read(id.clone()).await?.report, stable_report);
    assert_eq!(provider.requests().await.len(), 3);
    // A distinct audit demonstrates whole-audit Abort preserving a completed peer and a real partial.
    let mut abort_args = args();
    abort_args.reviewers.as_mut().unwrap()[0].provider = "completed_fixture".into();
    abort_args.reviewers.as_mut().unwrap()[2].provider = "audit_fixture".into();
    let aborted_initial = start_confirmed(&session, thread, abort_args, Some(true))
        .await?
        .unwrap();
    let abort_id = aborted_initial.run_id.clone();
    wait_attempts(&audits, &abort_id, |rows| {
        rows[0].completed && rows[1..].iter().all(|a| a.output_sequence > 0)
    })
    .await?;
    let (events, mut receive) = sender();
    let task = tokio::spawn(execute(
        session.request_handle(),
        thread,
        AuditAction::Abort(abort_id.clone()),
        events,
    ));
    let AppEvent::AuditAbortConfirm {
        run_id, decision, ..
    } = receive.recv().await.unwrap()
    else {
        panic!("whole-audit confirmation")
    };
    assert_eq!(run_id, abort_id);
    assert!(!audits.read(abort_id.clone()).await?.cancellation_requested);
    decision.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(3), task)
        .await??
        .map_err(color_eyre::eyre::Report::msg)?;
    let aborted = published(&audits, &abort_id).await?;
    assert_eq!(aborted.outcome.as_deref(), Some("aborted_by_operator"));
    let report = aborted.report.as_ref().unwrap();
    assert!(report.contains("COMPLETED_REVIEW_PEER"));
    assert!(report.contains("INCOMPLETE_NATIVE_PREFIX"));
    let (events, mut receive) = sender();
    observe(
        session.request_handle(),
        thread,
        aborted.clone(),
        events,
        Arc::new(tokio::sync::Notify::new()),
    )
    .await;
    let AppEvent::AuditUpdated { run, .. } = receive.recv().await.unwrap() else {
        panic!("published report display")
    };
    let mut contexts = ReportContexts::default();
    contexts.stage(thread, &run);
    assert_eq!(
        contexts
            .for_turn(thread)
            .unwrap()
            .values()
            .next()
            .unwrap()
            .kind,
        AdditionalContextKind::Untrusted
    );
    assert_eq!(provider.requests().await.len(), 5);
    assert_eq!(completed.received_requests().await.unwrap().len(), 1);
    if let Some(directory) = std::env::var_os("AUDEX_S3_EVIDENCE") {
        let directory = std::path::PathBuf::from(directory);
        std::fs::create_dir_all(&directory)?;
        std::fs::write(
            directory.join("process.json"),
            serde_json::to_vec_pretty(
                &json!({"fixtureHome":fixture_home,"pid":daemon.pid("app-server.pid").map_err(|error|color_eyre::eyre::eyre!("{error:#}"))?,"run":ready,"abortedRun":aborted,"originalAttempts":original,"retainedAttempts":audits.attempts(id).await?.attempts,"incompleteProviderRequests":5,"completedProviderRequests":1,"readinessProviderRequests":readiness_requests.load(std::sync::atomic::Ordering::SeqCst)}),
            )?,
        )?;
        std::fs::write(directory.join("aborted-report.md"), report)?;
    }
    session.shutdown().await?;
    daemon
        .lifecycle("stop")
        .map_err(|error| color_eyre::eyre::eyre!("{error:#}"))?;
    drop(releases);
    provider.shutdown().await;
    readiness_server.abort();
    let _ = readiness_server.await;
    Ok(())
}
