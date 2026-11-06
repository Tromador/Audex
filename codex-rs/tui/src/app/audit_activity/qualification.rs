//! S5 qualification only: actual CLI, main TUI, contextual keys, managed daemon and native loop.
use super::*;
use crate::app_server_session::ThreadParamsMode;
use crate::legacy_core::config::{ConfigBuilder, ConfigOverrides};
use codex_app_server_client::audit::AuditClient;
use codex_app_server_protocol::{AuditAttemptReadParams, AuditAttemptView, AuditRunView};
use codex_config::LoaderOverrides;
use core_test_support::responses;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};
use tokio::sync::{Semaphore, mpsc};

#[path = "../../../../cli/tests/app_server_daemon/support.rs"]
mod support;

#[path = "qualification_diagnostics.rs"]
mod diagnostics;

const SOURCE: &str = "fn example() { /* sealed source */ }\n";
const BRIEF: &str = "S5_SHARED_BRIEF";
const CONTEXT: &str = "S5_EXPLICIT_CONTEXT";
fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn evidence(name: &str, value: &Value) {
    let root = PathBuf::from(std::env::var_os("AUDEX_S5_EVIDENCE").expect("S5 evidence root"));
    std::fs::write(
        root.join(format!("{name}.json")),
        serde_json::to_vec_pretty(value).unwrap(),
    )
    .unwrap();
}
fn cli(daemon: &support::TestDaemon, args: &[&str], code: i32) -> Output {
    let output = diagnostics::cli(daemon, args, code);
    assert_eq!(
        output.status.code(),
        Some(code),
        "args={args:?}, stdout={}, stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}
fn sql(home: &Path, statement: &str, args: Value) -> Value {
    let script = "import sqlite3,json,sys; c=sqlite3.connect(sys.argv[1],timeout=5); c.row_factory=sqlite3.Row; q=c.execute(sys.argv[2],json.loads(sys.argv[3])); rows=[dict(r) for r in q] if q.description else []; c.commit(); print(json.dumps(rows)); c.close()";
    let output = Command::new("python3")
        .args(["-c", script])
        .arg(home.join("audex/audit-state.sqlite3"))
        .arg(statement)
        .arg(args.to_string())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "fixture SQL: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}
fn report_state(home: &Path, run: &str) -> Value {
    sql(home,"SELECT r.id,r.phase,r.collection_ready,r.control_intent,r.authoritative_selection_id,r.selection_generation,r.current_report_id,p.input_digest,p.content_digest,p.relative_path FROM audit_runs r LEFT JOIN reports p ON p.id=r.current_report_id WHERE r.id=?",json!([run]))[0].clone()
}
fn assignment(body: &Value) -> Value {
    body["input"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|item| item["content"].as_array())
        .flatten()
        .filter_map(|part| part["text"].as_str())
        .filter_map(|text| serde_json::from_str::<Value>(text).ok())
        .find(|value| value.get("assignment_id").is_some())
        .unwrap()
}
struct Provider {
    url: String,
    requests: Arc<Mutex<Vec<Value>>>,
    gates: Arc<Mutex<std::collections::HashMap<String, Arc<Semaphore>>>>,
    task: tokio::task::JoinHandle<()>,
}
impl Provider {
    async fn start() -> Result<Self> {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let gates = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let log = requests.clone();
        let held = gates.clone();
        let router=axum::Router::new().route("/v1/responses",axum::routing::post(move |axum::Json(body):axum::Json<Value>| {
            let log=log.clone();let held=held.clone();
            async move {
                use futures::StreamExt;
                let retained=assignment(&body);let model=body["model"].as_str().unwrap().to_owned();let id=retained["assignment_id"].as_str().unwrap().to_owned();
                let text=body.to_string();assert!(text.contains(BRIEF)&&text.contains(CONTEXT));assert!(!text.contains("S5_HOST_FORBIDDEN")&&!text.contains("S5_AMBIENT_FORBIDDEN"));
                log.lock().unwrap().push(body.clone());
                let continuation=body["input"].as_array().unwrap().iter().any(|item|item["type"]=="function_call_output");
                let response_id=format!("{id}-{}",if continuation {"final"} else {"source"});
                let mut created=responses::ev_response_created(&response_id);created["response"]["headers"]=json!({"OpenAI-Model":model});
                let mut gate=None;
                let initial=if !continuation {
                    assert!(!text.contains(SOURCE.trim()));
                    responses::sse(vec![created,responses::ev_function_call_with_namespace("source-read","audex_source","read",r#"{"path":"example.rs","offset":0,"max_bytes":128}"#),responses::ev_completed(&response_id)])
                } else {
                    assert!(text.contains(SOURCE.trim()));assert!(!text.contains("ALTERED_AFTER_SNAPSHOT"));
                    if model!="s5-findings" {let signal=Arc::new(Semaphore::new(0));held.lock().unwrap().insert(id.clone(),signal.clone());gate=Some(signal);}
                    responses::sse(vec![created,responses::ev_message_item_added("partial",""),responses::ev_output_text_delta(&format!("{{\"unfinished\":\"S5_PREFIX_{id}"))])
                };
                let mut result=json!({"schema_version":1,"snapshot_id":retained["snapshot_id"],"assignment_id":id,"summary":if model=="s5-findings" {"S5_FINDINGS_CANONICAL"} else {"S5_ZERO_CANONICAL"},"findings":[]});
                if model=="s5-findings" {result["findings"]=json!([{"severity":"high","confidence":"medium","title":"S5_FINDING_TITLE","mechanism":"Deterministic fixture mechanism","impact":"Deterministic impact","remediation":null,"evidence":[{"description":"Sealed source","path":"example.rs","start_byte":0,"end_byte":2,"start_line":1,"end_line":1}]}]);}
                let final_events=if continuation {responses::sse(vec![responses::ev_assistant_message("canonical",&result.to_string()),responses::ev_completed(&response_id)])} else {String::new()};
                let stream=futures::stream::iter([Ok::<_,std::convert::Infallible>(axum::body::Bytes::from(initial))]).chain(futures::stream::once(async move {if let Some(gate)=gate {let _=gate.acquire().await;}Ok(axum::body::Bytes::from(final_events))}));
                ([("content-type","text/event-stream".to_owned()),("OpenAI-Model",model)],axum::body::Body::from_stream(stream))
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}", listener.local_addr()?);
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        Ok(Self {
            url,
            requests,
            gates,
            task,
        })
    }
    fn count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
    fn release(&self, id: &str) {
        self.gates
            .lock()
            .unwrap()
            .get(id)
            .expect("provider boundary reached")
            .add_permits(1);
    }
}
impl Drop for Provider {
    fn drop(&mut self) {
        if let Ok(requests) = self.requests.lock() {
            evidence(
                &format!("provider-{}", uuid::Uuid::new_v4()),
                &json!({"requests":*requests,"count":requests.len()}),
            );
        }
        self.task.abort();
    }
}
struct Ui {
    app: Box<App>,
    tui: tui::Tui,
    session: AppServerSession,
    events: mpsc::UnboundedReceiver<AppEvent>,
    thread: ThreadId,
}
impl Ui {
    async fn connect(home: &Path) -> Result<Self> {
        let advertised = codex_app_server_client::app_server_control_socket_path(home)?;
        let socket =
            codex_app_server_client::verified_local_daemon_socket(advertised.as_path())?.unwrap();
        let client = crate::connect_remote_app_server(
            codex_app_server_client::RemoteAppServerEndpoint::UnixSocket {
                socket_path: socket.clone(),
            },
        )
        .await?;
        let mut session = AppServerSession::new(
            crate::backend_id::BackendId::new(),
            client,
            ThreadParamsMode::Remote,
        );
        let config = ConfigBuilder::default()
            .codex_home(home.to_owned())
            .harness_overrides(ConfigOverrides {
                cwd: Some(home.to_owned()),
                ..Default::default()
            })
            .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
            .build()
            .await?;
        let thread = session.start_thread(&config).await?.session.thread_id;
        let (mut app, events, mut ops) = crate::app::tests::make_test_app_with_channels().await;
        app.active_thread_id = Some(thread);
        app.primary_thread_id = Some(thread);
        app.app_server_target = AppServerTarget::LocalDaemon {
            endpoint: codex_app_server_client::RemoteAppServerEndpoint::UnixSocket {
                socket_path: socket,
            },
            allow_embedded_fallback: false,
        };
        app.chat_widget
            .handle_thread_session(crate::app::tests::test_thread_session(
                thread,
                home.to_owned(),
            ));
        while let Ok(op) = ops.try_recv() {
            assert!(matches!(
                op,
                crate::app_command::AppCommand::ListSkills { .. }
            ));
        }
        Ok(Self {
            app,
            tui: crate::tui::test_support::make_test_tui()?,
            session,
            events,
            thread,
        })
    }
    fn audits(&self) -> AuditClient {
        AuditClient::new(self.session.request_handle())
    }
    async fn dispatch(&mut self, event: AppEvent) -> Result<()> {
        self.app
            .handle_event(&mut self.tui, &mut self.session, event)
            .await?;
        Ok(())
    }
    async fn event(&mut self, which: fn(&AppEvent) -> bool) -> Result<AppEvent> {
        loop {
            let event = tokio::time::timeout(Duration::from_secs(20), self.events.recv())
                .await?
                .unwrap();
            if which(&event) {
                return Ok(event);
            }
            self.dispatch(event).await?;
        }
    }
    async fn pump(&mut self) -> Result<()> {
        while let Ok(event) = self.events.try_recv() {
            self.dispatch(event).await?;
        }
        Ok(())
    }
    async fn key(&mut self, code: KeyCode) -> Result<()> {
        self.app
            .handle_tui_event(
                &mut self.tui,
                &mut self.session,
                TuiEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)),
            )
            .await?;
        Ok(())
    }
    async fn yes(&mut self) -> Result<()> {
        self.key(KeyCode::Down).await?;
        self.key(KeyCode::Enter).await
    }
    async fn command(&mut self, args: String) -> Result<()> {
        self.dispatch(AppEvent::AuditCommand {
            thread_id: self.thread,
            args,
        })
        .await
    }
    async fn main_control(
        &mut self,
        run: &str,
        target: &str,
        action: &str,
    ) -> Result<crate::audit::AttemptControlConfirmation> {
        self.command(format!("attempt {action} {run} {target}"))
            .await?;
        self.confirmation().await
    }
    async fn confirmation(&mut self) -> Result<crate::audit::AttemptControlConfirmation> {
        let event = self
            .event(|e| matches!(e, AppEvent::AuditAttemptConfirm { .. }))
            .await?;
        let target = if let AppEvent::AuditAttemptConfirm { confirmation, .. } = &event {
            confirmation.clone()
        } else {
            unreachable!()
        };
        self.dispatch(event).await?;
        Ok(target)
    }
    async fn viewer(&mut self, target: AuditAttemptView) -> Result<()> {
        self.dispatch(AppEvent::AuditActivityOpen {
            thread_id: self.thread,
            attempt: target,
        })
        .await?;
        let page = self
            .event(|e| matches!(e, AppEvent::AuditActivityPage { .. }))
            .await?;
        self.dispatch(page).await
    }
    async fn contextual(&mut self, key: char) -> Result<crate::audit::AttemptControlConfirmation> {
        self.key(KeyCode::Char(key)).await?;
        self.confirmation().await
    }
    async fn finished(&mut self, success: bool) -> Result<()> {
        let event = self
            .event(|e| matches!(e, AppEvent::AuditActivityControlFinished { .. }))
            .await?;
        assert!(
            matches!(&event,AppEvent::AuditActivityControlFinished{result,..} if result.is_ok()==success)
        );
        self.dispatch(event).await
    }
    async fn close_viewer(&mut self) -> Result<()> {
        if self.app.overlay.is_some() {
            self.key(KeyCode::Esc).await?;
        }
        assert!(self.app.overlay.is_none());
        Ok(())
    }
    async fn start_tui(&mut self, args: String, accept: bool) -> Result<Option<AuditRunView>> {
        self.command(args).await?;
        let event = self
            .event(|e| matches!(e, AppEvent::AuditConfirm { .. }))
            .await?;
        assert!(
            matches!(&event,AppEvent::AuditConfirm{params,..} if params.reviewers.as_ref().unwrap().len()==3 && params.brief==BRIEF)
        );
        self.dispatch(event).await?;
        if !accept {
            self.key(KeyCode::Enter).await?;
            let event = self
                .event(|e| matches!(e, AppEvent::AuditError { .. }))
                .await?;
            self.dispatch(event).await?;
            return Ok(None);
        }
        self.yes().await?;
        let event = self
            .event(|e| matches!(e, AppEvent::AuditOpened { .. }))
            .await?;
        let run = if let AppEvent::AuditOpened { run, .. } = &event {
            run.clone()
        } else {
            unreachable!()
        };
        self.dispatch(event).await?;
        Ok(Some(run))
    }
}
async fn wait_attempts(
    audits: &AuditClient,
    run: &str,
    test: impl Fn(&[AuditAttemptView]) -> bool,
) -> Result<Vec<AuditAttemptView>> {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let items = audits.attempts(run.to_owned()).await?.attempts;
            if test(&items) {
                return Ok(items);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?
}
async fn captured(audits: &AuditClient, run: &str, id: &str) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let read = audits
                .read_attempt(AuditAttemptReadParams {
                    run_id: run.to_owned(),
                    assignment_id: id.to_owned(),
                    after_sequence: Some(0),
                    limit: Some(256),
                })
                .await?;
            if read.entries.iter().any(|entry| {
                entry.kind == "assistant_delta"
                    && entry.payload.contains(&format!("S5_PREFIX_{id}"))
            }) {
                return Ok::<_, color_eyre::Report>(());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?
}
async fn published(audits: &AuditClient, run: &str) -> Result<AuditRunView> {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let view = audits.read(run.to_owned()).await?;
            if crate::audit::terminal(&view) && view.report.is_some() {
                return Ok(view);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?
}
async fn candidate(home: &Path, run: &str) -> Result<(Value, Vec<u8>)> {
    tokio::time::timeout(Duration::from_secs(20),async{loop{
        let row=sql(home,"SELECT s.id,s.input_digest,s.input_json FROM audit_report_selections s JOIN audit_runs r ON r.authoritative_selection_id=s.id WHERE r.id=?",json!([run]));
        if let Some(row)=row.as_array().unwrap().first(){
            let input = row["input_json"].as_str().unwrap();
            let version = serde_json::from_str::<Value>(input)?["format_version"].as_u64().expect("report version");
            assert_eq!(version, 5);
            let report_digest=digest(format!("[{version},{input}]").as_bytes());
            let path=home.join("audex/reports").join(run).join(format!("{report_digest}.md"));
            if path.is_file(){let mut row=row.clone();row["reportInputDigest"]=json!(report_digest);return Ok((row,std::fs::read(path)?));}
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }}).await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn whole_cycle_cli_tui_contextual_controls_and_reporting_recover_without_replay() -> Result<()>
{
    // Construct heavyweight UI futures outside this long test body's poll frame.
    use crate::boxed_future;

    let provider = Provider::start().await?;
    let parent = PathBuf::from(std::env::var_os("AUDEX_TEST_TMPDIR").unwrap());
    let daemon =
        support::TestDaemon::new_in(&parent).map_err(|e| color_eyre::eyre::eyre!("{e:#}"))?;
    let home = daemon.home.path();
    std::fs::write(
        home.join("config.toml"),
        format!(
            r#"model="s5-findings"
model_provider="s5_fixture"
developer_instructions="S5_HOST_FORBIDDEN"
[features]
plugins=false
remote_plugin=false
plugin_discovery_prefetch=false
[model_providers.s5_fixture]
name="Owned S5 loopback native provider"
base_url="{}/v1"
wire_api="responses"
request_max_retries=0
stream_max_retries=0
"#,
            provider.url
        ),
    )?;
    std::fs::write(
        home.join("app-server-daemon/settings.json"),
        r#"{"shutdownGraceSeconds":0,"updater":{"autoUpdateEnabled":false}}"#,
    )?;
    std::fs::write(home.join("AGENTS.md"), "S5_AMBIENT_FORBIDDEN")?;
    std::fs::write(home.join("example.rs"), SOURCE)?;
    std::fs::write(home.join("guidance.md"), CONTEXT)?;
    daemon
        .lifecycle("start")
        .map_err(|e| color_eyre::eyre::eyre!("{e:#}"))?;
    let mut ui = boxed_future!(Ui::connect(home)).await?;
    let source = home.join("example.rs");
    let context = home.join("guidance.md");
    let start_args = [
        "--file",
        source.to_str().unwrap(),
        "--brief",
        BRIEF,
        "--audex-md",
        context.to_str().unwrap(),
        "--reviewer",
        "s5_fixture:s5-findings",
        "--reviewer",
        "s5_fixture:s5-zero",
        "--reviewer",
        "s5_fixture:s5-partial",
        "--detach",
        "--request-key",
        "s5-cli-roster",
    ];
    let started = cli(&daemon, &start_args, 0);
    let run = String::from_utf8(started.stdout)?.trim().to_owned();
    let replay = cli(&daemon, &start_args, 0);
    assert_eq!(String::from_utf8(replay.stdout)?.trim(), run);
    let targets = wait_attempts(&ui.audits(), &run, |items| {
        items.len() == 3 && items[0].completed && items[1..].iter().all(|a| a.output_sequence > 1)
    })
    .await?;
    for target in &targets[1..] {
        captured(&ui.audits(), &run, &target.assignment_id).await?;
    }
    assert_eq!(provider.count(), 6);
    std::fs::write(&source, "ALTERED_AFTER_SNAPSHOT")?;
    boxed_future!(ui.command(format!("reviewers {run}"))).await?;
    let listed =
        boxed_future!(ui.event(|e| matches!(e, AppEvent::AuditAttemptsLoaded { .. }))).await?;
    assert!(matches!(&listed,AppEvent::AuditAttemptsLoaded{attempts,..} if attempts.len()==3));
    boxed_future!(ui.dispatch(listed)).await?;
    let cancel = boxed_future!(ui.main_control(&run, &targets[1].assignment_id, "cancel")).await?;
    assert_eq!(cancel.target.assignment_id, targets[1].assignment_id);
    boxed_future!(ui.yes()).await?;
    let opened = boxed_future!(ui.event(|e| matches!(e, AppEvent::AuditOpened { .. }))).await?;
    boxed_future!(ui.dispatch(opened)).await?;
    let cancelled = ui
        .audits()
        .read_attempt(AuditAttemptReadParams {
            run_id: run.clone(),
            assignment_id: targets[1].assignment_id.clone(),
            after_sequence: Some(0),
            limit: Some(256),
        })
        .await?;
    assert!(cancelled.attempt.retry_eligible);
    boxed_future!(ui.viewer(cancelled.attempt.clone())).await?;
    let retry = boxed_future!(ui.contextual('R')).await?;
    assert!(retry.disclosure().contains("additional charges"));
    boxed_future!(ui.yes()).await?;
    let after_retry = wait_attempts(&ui.audits(), &run, |items| {
        items.len() == 4
            && items.iter().any(|a| {
                a.reviewer_ordinal == 1
                    && a.current
                    && a.assignment_id != targets[1].assignment_id
                    && a.output_sequence > 1
            })
    })
    .await?;
    let replacement = after_retry
        .iter()
        .find(|a| a.reviewer_ordinal == 1 && a.current)
        .unwrap()
        .clone();
    captured(&ui.audits(), &run, &replacement.assignment_id).await?;
    assert_eq!(provider.count(), 8);
    // The operator has not consumed the result yet. Replay the exact authorised action key.
    let replay = cli(
        &daemon,
        &[
            "attempt",
            "retry",
            &run,
            &retry.target.assignment_id,
            "--action-key",
            &retry.request.action_key,
            "--reason",
            &retry.request.reason,
            "--authorise-retry",
            "--acknowledge-possible-overlap",
        ],
        0,
    );
    assert!(String::from_utf8_lossy(&replay.stdout).contains(&replacement.assignment_id));
    assert_eq!(ui.audits().attempts(run.clone()).await?.attempts.len(), 4);
    assert_eq!(provider.count(), 8);
    boxed_future!(ui.finished(true)).await?;
    boxed_future!(ui.close_viewer()).await?;
    boxed_future!(ui.viewer(targets[2].clone())).await?;
    let actions_before = sql(
        home,
        "SELECT count(*) AS n FROM audit_attempt_actions",
        json!([]),
    );
    boxed_future!(ui.contextual('C')).await?;
    boxed_future!(ui.key(KeyCode::Enter)).await?;
    boxed_future!(ui.finished(false)).await?;
    assert_eq!(
        sql(
            home,
            "SELECT count(*) AS n FROM audit_attempt_actions",
            json!([])
        ),
        actions_before
    );
    boxed_future!(ui.contextual('C')).await?;
    boxed_future!(ui.yes()).await?;
    boxed_future!(ui.finished(true)).await?;
    let stopped = ui
        .audits()
        .read_attempt(AuditAttemptReadParams {
            run_id: run.clone(),
            assignment_id: targets[2].assignment_id.clone(),
            after_sequence: Some(0),
            limit: Some(256),
        })
        .await?;
    assert!(stopped.attempt.retry_eligible);
    boxed_future!(ui.close_viewer()).await?;
    boxed_future!(ui.viewer(stopped.attempt.clone())).await?;
    let stale = boxed_future!(ui.contextual('R')).await?;
    assert_eq!(stale.target.assignment_id, targets[2].assignment_id);
    provider.release(&replacement.assignment_id);
    let complete = published(&ui.audits(), &run).await?;
    let before_stale = report_state(home, &run);
    let report = complete.report.clone().unwrap();
    boxed_future!(ui.yes()).await?;
    boxed_future!(ui.finished(false)).await?;
    assert_eq!(report_state(home, &run), before_stale);
    assert_eq!(provider.count(), 8);
    boxed_future!(ui.close_viewer()).await?;
    boxed_future!(ui.command(format!("read {run}"))).await?;
    let opened = boxed_future!(ui.event(|e| matches!(e, AppEvent::AuditOpened { .. }))).await?;
    boxed_future!(ui.dispatch(opened)).await?;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            boxed_future!(ui.pump()).await?;
            if ui.app.audits.reports.for_turn(ui.thread).is_some() {
                return Ok::<_, color_eyre::Report>(());
            }
            tokio::task::yield_now().await;
        }
    })
    .await??;
    let staged = ui.app.audits.reports.for_turn(ui.thread).unwrap();
    assert!(staged.values().any(|v| v.value.contains(&report)));
    let read = cli(&daemon, &["read", &run], 0);
    assert_eq!(read.stdout, report.as_bytes());
    evidence(
        "first-published-report",
        &json!({"runId":run,"report":report,"state":before_stale,"providerRequestCount":provider.count()}),
    );
    assert!(
        report.contains(r"S5\_FINDING\_TITLE")
            && report.contains("No findings were reported.")
            && report.contains(&format!("S5_PREFIX_{}", targets[2].assignment_id))
    );
    assert!(!report.contains(&format!("S5_PREFIX_{}", targets[1].assignment_id)));
    assert!(
        report.find("## Reviewer 1").unwrap() < report.find("## Reviewer 2").unwrap()
            && report.find("## Reviewer 2").unwrap() < report.find("## Reviewer 3").unwrap()
    );
    let history = cli(
        &daemon,
        &[
            "attempt",
            "read",
            &run,
            &targets[1].assignment_id,
            "--limit",
            "256",
        ],
        0,
    );
    assert!(
        String::from_utf8_lossy(&history.stdout)
            .contains(&format!("S5_PREFIX_{}", targets[1].assignment_id))
    );
    evidence(
        "completed-workflow",
        &json!({"runId":run,"initialAttempts":targets,"replacement":replacement,"stoppedAttempt":stopped,"reportState":before_stale,"report":report,"reportBytesSha256":digest(&read.stdout),"historicalCli":serde_json::from_slice::<Value>(&history.stdout)?,"providerRequests":provider.requests.lock().unwrap().clone(),"stagedContext":staged}),
    );
    // A no-completed single reviewer proves final Cancel/exit 3 through actual CLI IPC.
    std::fs::write(&source, SOURCE)?;
    let out = cli(
        &daemon,
        &[
            "--file",
            source.to_str().unwrap(),
            "--brief",
            BRIEF,
            "--audex-md",
            context.to_str().unwrap(),
            "--model",
            "s5-partial",
            "--provider",
            "s5_fixture",
            "--detach",
            "--request-key",
            "s5-partial-only",
        ],
        0,
    );
    let partial_run = String::from_utf8(out.stdout)?.trim().to_owned();
    let partial = wait_attempts(&ui.audits(), &partial_run, |items| {
        items.len() == 1 && items[0].output_sequence > 1
    })
    .await?;
    captured(&ui.audits(), &partial_run, &partial[0].assignment_id).await?;
    cli(
        &daemon,
        &[
            "attempt",
            "cancel",
            &partial_run,
            &partial[0].assignment_id,
            "--action-key",
            "s5-last-cancel",
        ],
        0,
    );
    let partial_report = published(&ui.audits(), &partial_run).await?;
    let partial_read = cli(&daemon, &["read", &partial_run], 3);
    assert_eq!(
        partial_report.report.unwrap().as_bytes(),
        partial_read.stdout
    );
    cli(
        &daemon,
        &[
            "attempt",
            "retry",
            &partial_run,
            &partial[0].assignment_id,
            "--action-key",
            "s5-too-late",
            "--authorise-retry",
            "--acknowledge-possible-overlap",
        ],
        1,
    );
    evidence(
        "partial-only",
        &json!({"runId":partial_run,"report":String::from_utf8_lossy(&partial_read.stdout),"state":report_state(home,&partial_run)}),
    );
    // Main TUI commissioning, permanent Ignore, and a separate stale MAIN confirmation.
    let args = format!(
        "--snippet '{}' --name example.rs --brief {BRIEF} --audex-md '{}' --reviewer s5_fixture:s5-partial --reviewer s5_fixture:s5-partial --reviewer s5_fixture:s5-zero --request-key s5-tui-roster",
        SOURCE.trim(),
        context.display()
    );
    let count = provider.count();
    assert!(
        boxed_future!(ui.start_tui(args.clone(), false))
            .await?
            .is_none()
    );
    assert_eq!(provider.count(), count);
    let second = boxed_future!(ui.start_tui(args, true)).await?.unwrap();
    let other = wait_attempts(&ui.audits(), &second.run_id, |items| {
        items.len() == 3 && items.iter().all(|a| a.output_sequence > 1)
    })
    .await?;
    for target in &other {
        captured(&ui.audits(), &second.run_id, &target.assignment_id).await?;
    }
    boxed_future!(ui.viewer(other[0].clone())).await?;
    let ignored = boxed_future!(ui.contextual('I')).await?;
    assert!(ignored.disclosure().contains("permanently"));
    boxed_future!(ui.yes()).await?;
    boxed_future!(ui.finished(true)).await?;
    let ignored = ui
        .audits()
        .read_attempt(AuditAttemptReadParams {
            run_id: second.run_id.clone(),
            assignment_id: other[0].assignment_id.clone(),
            after_sequence: Some(0),
            limit: Some(256),
        })
        .await?;
    assert!(!ignored.attempt.retry_eligible);
    boxed_future!(ui.close_viewer()).await?;
    boxed_future!(ui.viewer(ignored.attempt.clone())).await?;
    if let Some(Overlay::AuditActivity(view)) = &ui.app.overlay {
        assert!(
            view.action_for_key(KeyEvent::new(KeyCode::Char('R'), KeyModifiers::NONE))
                .is_none()
        );
    }
    cli(
        &daemon,
        &[
            "attempt",
            "retry",
            &second.run_id,
            &other[0].assignment_id,
            "--action-key",
            "s5-ignored-retry",
            "--authorise-retry",
            "--acknowledge-possible-overlap",
        ],
        1,
    );
    boxed_future!(ui.close_viewer()).await?;
    boxed_future!(ui.main_control(&second.run_id, &other[1].assignment_id, "cancel")).await?;
    boxed_future!(ui.yes()).await?;
    let opened = boxed_future!(ui.event(|e| matches!(e, AppEvent::AuditOpened { .. }))).await?;
    boxed_future!(ui.dispatch(opened)).await?;
    let stale_main =
        boxed_future!(ui.main_control(&second.run_id, &other[1].assignment_id, "retry")).await?;
    sql(
        home,
        "CREATE TRIGGER s5_hold_publication BEFORE INSERT ON reports BEGIN SELECT RAISE(ABORT,'S5 publication gate'); END",
        json!([]),
    );
    provider.release(&other[2].assignment_id);
    let (normal, normal_bytes) = candidate(home, &second.run_id).await?;
    evidence(
        "normal-candidate",
        &json!({"runId":second.run_id,"selection":normal,"report":String::from_utf8_lossy(&normal_bytes),"providerRequestCount":provider.count()}),
    );
    let ready = report_state(home, &second.run_id);
    assert_eq!(ready["collection_ready"], 1);
    assert_eq!(ready["phase"], "reporting");
    boxed_future!(ui.yes()).await?;
    let rejected = boxed_future!(ui.event(|e| matches!(e, AppEvent::AuditError { .. }))).await?;
    boxed_future!(ui.dispatch(rejected)).await?;
    assert_eq!(stale_main.target.assignment_id, other[1].assignment_id);
    assert_eq!(report_state(home, &second.run_id), ready);
    let count = provider.count();
    cli(&daemon, &["read", &second.run_id], 5);
    boxed_future!(ui.command(format!("abort {}", second.run_id))).await?;
    let abort =
        boxed_future!(ui.event(|e| matches!(e, AppEvent::AuditAbortConfirm { .. }))).await?;
    boxed_future!(ui.dispatch(abort)).await?;
    let began = std::time::Instant::now();
    boxed_future!(ui.yes()).await?;
    let opened = boxed_future!(ui.event(|e| matches!(e, AppEvent::AuditOpened { .. }))).await?;
    let abort_ack_ms = began.elapsed().as_millis();
    assert!(abort_ack_ms < 3000);
    boxed_future!(ui.dispatch(opened)).await?;
    let (aborted, aborted_bytes) = candidate(home, &second.run_id).await?;
    assert_ne!(normal["id"], aborted["id"]);
    assert_ne!(normal_bytes, aborted_bytes);
    evidence(
        "aborted-candidate",
        &json!({"runId":second.run_id,"selection":aborted,"report":String::from_utf8_lossy(&aborted_bytes),"providerRequestCount":provider.count()}),
    );
    let before_crash = report_state(home, &second.run_id);
    assert_eq!(before_crash["selection_generation"], 2);
    // Kill only the fixture PID after proving command and CODEX_HOME ownership.
    let pid = daemon
        .pid("app-server.pid")
        .map_err(|e| color_eyre::eyre::eyre!("{e:#}"))?;
    let env = std::fs::read(format!("/proc/{pid}/environ"))?;
    let cmd = std::fs::read(format!("/proc/{pid}/cmdline"))?;
    assert!(
        env.split(|b| *b == 0)
            .any(|entry| entry == format!("CODEX_HOME={}", home.display()).as_bytes())
    );
    assert!(String::from_utf8_lossy(&cmd).contains("app-server"));
    drop(ui);
    support::signal(pid, 9).map_err(|e| color_eyre::eyre::eyre!("{e:#}"))?;
    support::wait_for_exit(pid).map_err(|e| color_eyre::eyre::eyre!("{e:#}"))?;
    daemon
        .lifecycle("start")
        .map_err(|e| color_eyre::eyre::eyre!("{e:#}"))?;
    let recovered = candidate(home, &second.run_id).await?;
    assert_eq!(recovered.0, aborted);
    assert_eq!(recovered.1, aborted_bytes);
    assert_eq!(provider.count(), count);
    cli(&daemon, &["read", &second.run_id], 5);
    let mut ui = boxed_future!(Ui::connect(home)).await?;
    let replayed = ui.audits().cancel(second.run_id.clone()).await?;
    assert_eq!(
        replayed.phase,
        codex_app_server_protocol::AuditRunPhase::Reporting
    );
    assert_eq!(report_state(home, &second.run_id), before_crash);
    sql(home, "DROP TRIGGER s5_hold_publication", json!([]));
    let terminal = published(&ui.audits(), &second.run_id).await?;
    let final_read = cli(&daemon, &["read", &second.run_id], 6);
    assert_eq!(final_read.stdout, aborted_bytes);
    assert_eq!(
        terminal.report.as_deref(),
        std::str::from_utf8(&aborted_bytes).ok()
    );
    let final_state = report_state(home, &second.run_id);
    assert_eq!(final_state["content_digest"], digest(&final_read.stdout));
    assert_ne!(final_state["authoritative_selection_id"], normal["id"]);
    assert_eq!(
        sql(
            home,
            "SELECT count(*) AS n FROM reports WHERE run_id=?",
            json!([second.run_id])
        )[0]["n"],
        1
    );
    let alias = cli(&daemon, &["cancel", &second.run_id], 6);
    assert_eq!(alias.stdout, final_read.stdout);
    assert_eq!(report_state(home, &second.run_id), final_state);
    boxed_future!(ui.command(format!("read {}", second.run_id))).await?;
    let opened = boxed_future!(ui.event(|e| matches!(e, AppEvent::AuditOpened { .. }))).await?;
    boxed_future!(ui.dispatch(opened)).await?;
    boxed_future!(ui.pump()).await?;
    let completed_after_restart = cli(&daemon, &["read", &run], 0);
    assert_eq!(completed_after_restart.stdout, read.stdout);
    for signal in provider.gates.lock().unwrap().values() {
        signal.add_permits(1);
    }
    assert_eq!(provider.count(), count);
    let after_release = cli(&daemon, &["read", &second.run_id], 6);
    assert_eq!(
        after_release.stdout,
        final_read.stdout,
        "final read: status={}, stdout={}, stderr={}",
        after_release.status,
        String::from_utf8_lossy(&after_release.stdout),
        String::from_utf8_lossy(&after_release.stderr)
    );
    evidence(
        "abort-recovery",
        &json!({"runId":second.run_id,"attempts":other,"normalSelection":normal,"normalCandidateSha256":digest(&normal_bytes),"abortedSelection":aborted,"abortedCandidateSha256":digest(&aborted_bytes),"beforeCrash":before_crash,"finalState":final_state,"report":terminal.report,"abortAcknowledgementMs":abort_ack_ms,"killedPid":pid,"boundary":"authoritative aborted candidate exists; terminal insertion blocked","providerRequestCountBeforeRestart":count,"providerRequestCountAfterRestart":provider.count(),"completedReportSha256AfterRestart":digest(&completed_after_restart.stdout)}),
    );
    ui.session.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn incompatible_protocol_refuses_start_abort_and_shared_attempt_controls_before_confirmation()
-> Result<()> {
    let mut cases = Vec::new();
    for version in [0, AUDIT_PROTOCOL_VERSION - 1, AUDIT_PROTOCOL_VERSION + 1] {
        for operation in ["start", "abort", "cancel", "ignore", "retry"] {
            let (cloud, mut commands, _incoming) = CloudClient::channel(8);
            let client = AppServerClient::Cloud(cloud);
            let handle = client.request_handle();
            let (tx, mut events) = mpsc::unbounded_channel();
            let sender = crate::app_event_sender::AppEventSender::new(tx);
            let task = tokio::spawn(async move {
                let thread = ThreadId::new();
                if matches!(operation, "start" | "abort") {
                    let args = if operation == "start" {
                        "--snippet source --name example.rs"
                    } else {
                        "abort fixture-run"
                    };
                    crate::audit::execute(
                        handle,
                        thread,
                        crate::audit::parse_command(args)?
                            .action(Path::new("."))
                            .await?,
                        sender,
                    )
                    .await
                    .map(|_| ())
                } else {
                    let action = match operation {
                        "cancel" => AuditAttemptAction::Cancel,
                        "ignore" => AuditAttemptAction::Ignore,
                        _ => AuditAttemptAction::Retry,
                    };
                    crate::audit::confirm_and_control_attempt(
                        &AuditClient::new(handle),
                        thread,
                        crate::audit::tests::attempt_fixture(),
                        action,
                        Some("s5-incompatible-action".into()),
                        &sender,
                    )
                    .await
                    .map(|_| ())
                }
            });
            let capability = tokio::time::timeout(Duration::from_secs(2), commands.recv())
                .await?
                .expect("compatibility request");
            assert!(matches!(
                capability.operation,
                CloudOperation::Request(request)
                    if matches!(*request, ClientRequest::AuditCapabilities { .. })
            ));
            capability
                .response
                .send(Ok(Ok(json!({"protocolVersion":version}))))
                .unwrap();
            let error = tokio::time::timeout(Duration::from_secs(2), task)
                .await??
                .expect_err("incompatible protocol must refuse");
            assert!(error.contains("incompatible Audex daemon protocol"));
            assert!(commands.try_recv().is_err(), "no start/control/inference");
            assert!(
                events.try_recv().is_err(),
                "no confirmation or history event"
            );
            cases.push(json!({"protocolVersion":version,"operation":operation,"capabilityRequests":1,"subsequentRequests":0,"error":error}));
        }
    }
    evidence("protocol-refusals", &json!({"cases":cases}));
    Ok(())
}
