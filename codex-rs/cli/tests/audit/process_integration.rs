//! S1 process verification. Fault gates and private roster rows exist only in owned fixture stores.
use super::*;
use codex_app_server_client::audit::AuditClient;
use codex_app_server_client::{
    AppServerRequestHandle, RemoteAppServerClient, RemoteAppServerConnectArgs,
    RemoteAppServerEndpoint,
};
use core_test_support::streaming_sse::{
    StreamingSseChunk, StreamingSseServer, start_streaming_sse_server,
};
use sha2::{Digest, Sha256};
use sqlx::Row;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tokio::sync::oneshot;

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

struct Case {
    daemon: TestDaemon,
    evidence: PathBuf,
    pids: Mutex<Vec<u32>>,
}
impl std::ops::Deref for Case {
    type Target = TestDaemon;
    fn deref(&self) -> &TestDaemon {
        &self.daemon
    }
}
impl Case {
    fn new(name: &str, url: &str) -> Result<Self> {
        let root = PathBuf::from(
            std::env::var_os("AUDEX_PROCESS_EVIDENCE_DIR")
                .context("set AUDEX_PROCESS_EVIDENCE_DIR")?,
        );
        let evidence = root.join(format!("{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&evidence)?;
        let result = Self {
            daemon: configured_url(url)?,
            evidence,
            pids: Mutex::new(Vec::new()),
        };
        result.event(
            json!({"event":"fixture","home":result.home.path(),"evidence":result.evidence}),
        )?;
        eprintln!("S1 evidence: {}", result.evidence.display());
        Ok(result)
    }
    fn event(&self, value: Value) -> Result<()> {
        writeln!(
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.evidence.join("events.jsonl"))?,
            "{value}"
        )?;
        Ok(())
    }
    fn current_pid(&self) -> Result<u32> {
        self.pid("app-server.pid").or_else(|_| {
            self.pids
                .lock()
                .expect("PIDs")
                .last()
                .copied()
                .map(Ok)
                .unwrap_or_else(|| {
                    Ok(std::fs::read_to_string(self.evidence.join("debugger.pid"))?
                        .parse::<u32>()?)
                })
        })
    }
    fn remember_pid(&self) -> Result<u32> {
        let pid = self.current_pid()?;
        ensure!(
            owned_pid(&self.daemon, pid)?,
            "refuse non-fixture PID {pid}"
        );
        self.pids.lock().expect("PIDs").push(pid);
        self.event(json!({"event":"daemon_pid","pid":pid}))?;
        Ok(pid)
    }
    fn start(&self) -> Result<()> {
        self.lifecycle("start")?;
        self.remember_pid()?;
        Ok(())
    }
    fn crash(&self, boundary: &str) -> Result<()> {
        let pid = self.remember_pid()?;
        self.event(json!({"event":"SIGKILL","pid":pid,"boundary":boundary}))?;
        signal(pid, libc::SIGKILL)?;
        // A debugger stopped in its own Python command must resume to reap its inferior.
        std::fs::write(self.evidence.join("freeze.release"), b"release")?;
        wait_for_exit(pid)?;
        Ok(())
    }
}
fn owned_pid(daemon: &TestDaemon, pid: u32) -> Result<bool> {
    #[cfg(target_os = "linux")]
    {
        let environ = match std::fs::read(format!("/proc/{pid}/environ")) {
            Ok(value) => value,
            Err(_) => return Ok(false),
        };
        let expected = format!("CODEX_HOME={}", daemon.home.path().display());
        let command = std::fs::read(format!("/proc/{pid}/cmdline"))?;
        return Ok(environ
            .split(|byte| *byte == 0)
            .any(|entry| entry == expected.as_bytes())
            && command
                .windows(b"app-server".len())
                .any(|part| part == b"app-server"));
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (daemon, pid);
        anyhow::bail!("owned-PID verification currently requires Linux")
    }
}
fn copy_evidence(source: &Path, target: &Path) -> std::io::Result<()> {
    let metadata = std::fs::symlink_metadata(source)?;
    if metadata.is_dir() {
        std::fs::create_dir_all(target)?;
        for entry in std::fs::read_dir(source)? {
            let entry = entry?;
            copy_evidence(&entry.path(), &target.join(entry.file_name()))?;
        }
    } else if metadata.is_file() {
        std::fs::copy(source, target)?;
    }
    Ok(())
}
impl Drop for Case {
    fn drop(&mut self) {
        // Only recorded, identity-checked fixture PIDs are eligible for cleanup.
        if let Ok(pid) = self.current_pid() {
            if owned_pid(&self.daemon, pid).unwrap_or(false) {
                let _ = self.event(json!({"event":"cleanup_SIGKILL","pid":pid}));
                let _ = signal(pid, libc::SIGKILL);
                let _ = std::fs::write(self.evidence.join("freeze.release"), b"release");
                let _ = wait_for_exit(pid);
            }
        }
        // Native Codex runtime sockets use a fixed protected OS directory.
        // Resolve only this fixture's verified alias; never scan/delete peers.
        if let Ok(advertised) =
            codex_app_server_client::app_server_control_socket_path(self.home.path())
        {
            if let Ok(Some(socket)) =
                codex_app_server_client::verified_local_daemon_socket(advertised.as_path())
            {
                let _ = self.event(json!({"event":"owned_socket_cleanup","path":socket}));
                let _ = std::fs::remove_file(socket.as_path());
            }
        }
        for name in ["audex", "app-server-daemon", "config.toml"] {
            let source = self.home.path().join(name);
            if source.exists() {
                let _ = copy_evidence(&source, &self.evidence.join(name));
            }
        }
        let _ =
            self.event(json!({"event":"cleanup","owned_pids":*self.pids.lock().expect("PIDs")}));
        // TestDaemon then reaps its direct child, removes owned socket metadata,
        // and restores sealed-directory owner permissions before TempDir cleanup.
    }
}

async fn db(case: &Case) -> Result<sqlx::SqlitePool> {
    fixture_db(case).await
}
async fn initialise(case: &Case) -> Result<sqlx::SqlitePool> {
    case.start()?;
    check(&audit(case).arg("list").output()?, 0)?;
    db(case).await
}
async fn gate(pool: &sqlx::SqlitePool, which: &str) -> Result<()> {
    let sql = match which {
        "freeze" => {
            "CREATE TRIGGER s1_hold_freeze BEFORE INSERT ON audit_report_selections BEGIN SELECT RAISE(ABORT,'S1 fixture freeze gate'); END"
        }
        "publication" => {
            "CREATE TRIGGER s1_hold_publication BEFORE INSERT ON reports BEGIN SELECT RAISE(ABORT,'S1 fixture publication gate'); END"
        }
        "commission" => {
            "CREATE TRIGGER s1_hold_commission BEFORE UPDATE OF phase ON audit_runs WHEN NEW.phase='snapshot_bound' BEGIN SELECT RAISE(ABORT,'S1 fixture private roster gate'); END"
        }
        _ => anyhow::bail!("unknown gate"),
    };
    sqlx::query(sql).execute(pool).await?;
    Ok(())
}
async fn ungate(pool: &sqlx::SqlitePool, which: &str) -> Result<()> {
    let sql = match which {
        "freeze" => "DROP TRIGGER s1_hold_freeze",
        "publication" => "DROP TRIGGER s1_hold_publication",
        "commission" => "DROP TRIGGER s1_hold_commission",
        _ => anyhow::bail!("unknown gate"),
    };
    sqlx::query(sql).execute(pool).await?;
    Ok(())
}
fn start_run(case: &Case) -> Result<String> {
    let source = case.home.path().join("example.rs");
    std::fs::write(&source, b"fn main() {}\n")?;
    let output = audit(case)
        .arg("--file")
        .arg(source)
        .arg("--detach")
        .output()?;
    check(&output, 0)?;
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}
async fn wait_state(pool: &sqlx::SqlitePool, run: &str, selected: bool) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(30),async {
        loop {
            let (ready,selection,published):(bool,Option<String>,Option<String>)=sqlx::query_as("SELECT collection_ready,authoritative_selection_id,current_report_id FROM audit_runs WHERE id=?").bind(run).fetch_one(pool).await?;
            ensure!(published.is_none(),"publication escaped fixture gate");
            if ready && selection.is_some()==selected { return anyhow::Ok(()); }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }).await??;
    Ok(())
}
async fn snapshot(case: &Case, pool: &sqlx::SqlitePool, run: &str, label: &str) -> Result<Value> {
    let row=sqlx::query("SELECT phase,outcome,collection_ready,authoritative_selection_id,current_report_id,selection_generation,stop_generation FROM audit_runs WHERE id=?").bind(run).fetch_one(pool).await?;
    let selections:Vec<(String,String,String)>=sqlx::query_as("SELECT id,input_digest,input_json FROM audit_report_selections WHERE run_id=? ORDER BY generation").bind(run).fetch_all(pool).await?;
    let reports:Vec<(String,String,String,String)>=sqlx::query_as("SELECT id,input_digest,content_digest,relative_path FROM reports WHERE run_id=? ORDER BY id").bind(run).fetch_all(pool).await?;
    let attempts:Vec<(String,String,String,Option<i64>,Option<String>)>=sqlx::query_as("SELECT a.id,a.state,t.disposition,t.output_cutoff,a.result_digest FROM reviewer_assignments a JOIN audit_attempts t ON t.assignment_id=a.id WHERE a.run_id=? ORDER BY a.ordinal").bind(run).fetch_all(pool).await?;
    let value = json!({"event":"state","label":label,"run":run,"phase":row.try_get::<String,_>("phase")?,"outcome":row.try_get::<Option<String>,_>("outcome")?,
        "ready":row.try_get::<bool,_>("collection_ready")?,"authoritative_selection":row.try_get::<Option<String>,_>("authoritative_selection_id")?,
        "current_report":row.try_get::<Option<String>,_>("current_report_id")?,"generation":row.try_get::<i64,_>("selection_generation")?,"stop_generation":row.try_get::<i64,_>("stop_generation")?,
        "selections":selections,"reports":reports,"attempts":attempts});
    case.event(value.clone())?;
    Ok(value)
}
struct Candidate {
    selection: String,
    id: String,
    input_digest: String,
    content_digest: String,
    bytes: Vec<u8>,
}
async fn candidate(case: &Case, pool: &sqlx::SqlitePool, run: &str) -> Result<Candidate> {
    tokio::time::timeout(Duration::from_secs(30),async {
        loop {
            let row:Option<(String,String)>=sqlx::query_as("SELECT s.id,s.input_json FROM audit_runs r JOIN audit_report_selections s ON s.id=r.authoritative_selection_id WHERE r.id=?").bind(run).fetch_optional(pool).await?;
            if let Some((selection,json))=row {
                // The retained JSON is the canonical struct serialisation; parsing
                // it into a sorted Value would change the versioned input bytes.
                let version=serde_json::from_str::<Value>(&json)?["format_version"].as_u64().context("report version")?;
                let input_digest=digest(format!("[{version},{json}]").as_bytes());
                let path=case.home.path().join(format!("audex/reports/{run}/{input_digest}.md"));
                if let Ok(bytes)=std::fs::read(path) {
                    let result=Candidate { selection,id:format!("audex-report-v{version}:sha256:{input_digest}"),input_digest,content_digest:digest(&bytes),bytes };
                    std::fs::write(case.evidence.join(format!("candidate-{}.md",result.input_digest)),&result.bytes)?;
                    case.event(json!({"event":"candidate","selection":result.selection,"report_id":result.id,"input_digest":result.input_digest,"content_digest":result.content_digest}))?;
                    return Ok::<_,anyhow::Error>(result);
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }).await?
}
async fn assert_final(
    case: &Case,
    pool: &sqlx::SqlitePool,
    run: &str,
    code: i32,
    expected: Option<&Candidate>,
) -> Result<Vec<u8>> {
    let mut command = tokio::process::Command::from(audit(case));
    let output = tokio::time::timeout(
        Duration::from_secs(30),
        command
            .args(["read", run, "--follow"])
            .kill_on_drop(true)
            .output(),
    )
    .await??;
    check(&output, code)?;
    let row:(String,String,String)=sqlx::query_as("SELECT p.id,p.input_digest,p.content_digest FROM reports p JOIN audit_runs r ON r.current_report_id=p.id WHERE r.id=?").bind(run).fetch_one(pool).await?;
    assert_eq!(row.2, digest(&output.stdout));
    if let Some(expected) = expected {
        assert_eq!(
            row,
            (
                expected.id.clone(),
                expected.input_digest.clone(),
                expected.content_digest.clone()
            )
        );
        assert_eq!(output.stdout, expected.bytes);
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM reports WHERE run_id=?")
        .bind(run)
        .fetch_one(pool)
        .await?;
    assert_eq!(count, 1);
    snapshot(case, pool, run, "published").await?;
    case.event(json!({"event":"final_cli","exit":code,"sha256":digest(&output.stdout)}))?;
    Ok(output.stdout)
}
async fn retry_refused(case: &Case, pool: &sqlx::SqlitePool, run: &str) -> Result<()> {
    let assignment: String = sqlx::query_scalar(
        "SELECT current_assignment_id FROM audit_reviewers WHERE run_id=? AND ordinal=0",
    )
    .bind(run)
    .fetch_one(pool)
    .await?;
    let before:(i64,i64)=sqlx::query_as("SELECT (SELECT count(*) FROM reviewer_assignments WHERE run_id=?),(SELECT count(*) FROM audit_attempt_actions)").bind(run).fetch_one(pool).await?;
    let output = audit(case)
        .args([
            "attempt",
            "retry",
            run,
            &assignment,
            "--authorise-retry",
            "--acknowledge-possible-overlap",
            "--action-key",
            "post-readiness-process-retry",
        ])
        .output()?;
    check(&output, 1)?;
    let after:(i64,i64)=sqlx::query_as("SELECT (SELECT count(*) FROM reviewer_assignments WHERE run_id=?),(SELECT count(*) FROM audit_attempt_actions)").bind(run).fetch_one(pool).await?;
    assert_eq!(before, after);
    case.event(json!({"event":"retry_refused","exit":1,"counts":after}))?;
    Ok(())
}
async fn abort_ack(case: &Case, run: &str) -> Result<()> {
    let advertised = codex_app_server_client::app_server_control_socket_path(case.home.path())?;
    let socket_path = codex_app_server_client::verified_local_daemon_socket(advertised.as_path())?
        .context("owned daemon socket")?;
    let connection = RemoteAppServerClient::connect(RemoteAppServerConnectArgs {
        required_human_approval: false,
        endpoint: RemoteAppServerEndpoint::UnixSocket { socket_path },
        client_name: "s1-process-fixture".to_owned(),
        client_version: "0.0.0".to_owned(),
        experimental_api: true,
        mcp_server_openai_form_elicitation: false,
        opt_out_notification_methods: vec![],
        channel_capacity: 32,
    })
    .await?;
    let audits = AuditClient::new(AppServerRequestHandle::Remote(connection.request_handle()));
    audits.check_compatibility().await?;
    let began = std::time::Instant::now();
    let ack = tokio::time::timeout(Duration::from_secs(3), audits.cancel(run.to_owned())).await??;
    assert_eq!(
        ack.phase,
        codex_app_server_protocol::AuditRunPhase::Reporting
    );
    assert!(ack.report.is_none());
    case.event(json!({"event":"durable_abort_ack","elapsed_ms":began.elapsed().as_millis(),"revision":ack.revision}))?;
    connection.shutdown().await?;
    Ok(())
}
async fn partial_server() -> (StreamingSseServer, oneshot::Sender<()>) {
    let (release, gate) = oneshot::channel();
    let (server, _) = start_streaming_sse_server(vec![vec![
        StreamingSseChunk {
            gate: None,
            body: responses::sse(vec![
                responses::ev_response_created("partial"),
                responses::ev_message_item_added("partial", ""),
                responses::ev_output_text_delta("PRE_STOP_PARTIAL_UNFINISHED {"),
            ]),
        },
        StreamingSseChunk {
            gate: Some(gate),
            body: responses::sse(vec![
                responses::ev_output_text_delta("POST_STOP_FORBIDDEN"),
                responses::ev_completed("partial"),
            ]),
        },
    ]])
    .await;
    (server, release)
}
async fn wait_partial(case: &Case, run: &str) -> Result<String> {
    let list = audit(case).args(["attempt", "list", run]).output()?;
    check(&list, 0)?;
    let list: Value = serde_json::from_slice(&list.stdout)?;
    let assignment = list["attempts"][0]["assignmentId"]
        .as_str()
        .context("assignment")?
        .to_owned();
    super::attempt_controls::wait_text(case, run, &assignment, "PRE_STOP_PARTIAL_UNFINISHED {")
        .await?;
    Ok(assignment)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn abort_alias_pending_ack_follow_and_frontend_disconnects() -> Result<()> {
    for action in ["abort", "cancel"] {
        let (server, release) = partial_server().await;
        let case = Case::new(&format!("cli-{action}"), &server.uri())?;
        let pool = initialise(&case).await?;
        gate(&pool, "publication").await?;
        let run = start_run(&case)?;
        wait_partial(&case, &run).await?;
        // Kill one follower and Ctrl-C another: neither is an audit control.
        for signum in [libc::SIGKILL, libc::SIGINT] {
            let mut follower = tokio::process::Command::from(audit(&case))
                .args(["read", &run, "--follow"])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .spawn()?;
            let mut lines = tokio::io::AsyncBufReadExt::lines(tokio::io::BufReader::new(
                follower.stderr.take().context("follower stderr")?,
            ));
            tokio::time::timeout(Duration::from_secs(10), async {
                while let Some(line) = lines.next_line().await? {
                    if line.contains(&run) {
                        return anyhow::Ok(());
                    }
                }
                anyhow::bail!("follower never attached")
            })
            .await??;
            signal(follower.id().context("owned follower")?, signum)?;
            let status = tokio::time::timeout(Duration::from_secs(10), follower.wait()).await??;
            if signum == libc::SIGINT {
                assert_eq!(status.code(), Some(130));
            }
            let intent: String =
                sqlx::query_scalar("SELECT control_intent FROM audit_runs WHERE id=?")
                    .bind(&run)
                    .fetch_one(&pool)
                    .await?;
            assert_eq!(intent, "active");
            check(&audit(&case).args(["read", &run]).output()?, 5)?;
            case.event(json!({"event":"frontend_disconnect","signal":signum,"exit":status.code(),"audit_intent":intent}))?;
        }
        let began = std::time::Instant::now();
        let mut abort = tokio::process::Command::from(audit(&case))
            .args([action, &run])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let mut progress = tokio::io::AsyncBufReadExt::lines(tokio::io::BufReader::new(
            abort.stderr.take().context("Abort stderr")?,
        ));
        let acknowledged = tokio::time::timeout(Duration::from_secs(3), async {
            while let Some(line) = progress.next_line().await? {
                if line.contains("cancellation requested") {
                    return Ok::<_, anyhow::Error>(line);
                }
            }
            anyhow::bail!("Abort closed without its durable acknowledgement")
        })
        .await??;
        case.event(json!({"event":"first_cli_abort_ack","action":action,"elapsed_ms":began.elapsed().as_millis(),"progress":acknowledged}))?;
        wait_state(&pool, &run, true).await?;
        // The durable Abort is already recorded; repeated RPC must promptly ack
        // while CLI remains attached waiting for the independently gated report.
        abort_ack(&case, &run).await?;
        assert!(abort.try_wait()?.is_none());
        check(&audit(&case).args(["read", &run]).output()?, 5)?;
        let expected = candidate(&case, &pool, &run).await?;
        assert!(String::from_utf8_lossy(&expected.bytes).contains("PRE_STOP_PARTIAL_UNFINISHED {"));
        snapshot(&case, &pool, &run, "aborted_candidate_unpublished").await?;
        case.crash("aborted candidate exists; terminal publication gated")
            .context("owned daemon crash")?;
        let disconnected =
            tokio::time::timeout(Duration::from_secs(10), abort.wait_with_output()).await??;
        check(&disconnected, 1)?; // Actual daemon disappearance is a transport error.
        ungate(&pool, "publication").await?;
        case.start()?;
        let bytes = assert_final(&case, &pool, &run, 6, Some(&expected)).await?;
        let state = snapshot(&case, &pool, &run, "before_replayed_abort").await?;
        check(&audit(&case).args([action, &run]).output()?, 6)?;
        same_state(
            snapshot(&case, &pool, &run, "after_replayed_abort").await?,
            state,
        );
        let _ = release.send(());
        case.crash("terminal aborted publication")
            .context("terminal restart")?;
        case.start()?;
        assert_eq!(
            assert_final(&case, &pool, &run, 6, Some(&expected)).await?,
            bytes
        );
        assert_eq!(server.requests().await.len(), 1);
        case.event(json!({"event":"provider_requests","count":1}))?;
        pool.close().await;
        server.shutdown().await;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn readiness_before_freeze_survives_sigkill_and_retry_stays_closed() -> Result<()> {
    let (server, release) = partial_server().await;
    let case = Case::new("readiness", &server.uri())?;
    let pool = initialise(&case).await?;
    gate(&pool, "freeze").await?;
    let run = start_run(&case)?;
    let assignment = wait_partial(&case, &run).await?;
    check(
        &audit(&case)
            .args([
                "attempt",
                "cancel",
                &run,
                &assignment,
                "--action-key",
                "last-contribution",
            ])
            .output()?,
        0,
    )?;
    wait_state(&pool, &run, false).await?;
    snapshot(&case, &pool, &run, "ready_no_frozen_selection").await?;
    retry_refused(&case, &pool, &run).await?;
    case.crash("readiness committed; freeze transaction blocked by fixture trigger")?;
    case.start()?;
    wait_state(&pool, &run, false).await?;
    retry_refused(&case, &pool, &run).await?;
    check(&audit(&case).args(["read", &run]).output()?, 5)?;
    case.crash("recovered readiness; freeze still gated")?;
    ungate(&pool, "freeze").await?;
    case.start()?;
    let bytes = assert_final(&case, &pool, &run, 3, None).await?;
    assert!(String::from_utf8_lossy(&bytes).contains("PRE_STOP_PARTIAL_UNFINISHED {"));
    assert!(!String::from_utf8_lossy(&bytes).contains("POST_STOP_FORBIDDEN"));
    retry_refused(&case, &pool, &run).await?;
    case.crash("terminal no-completed-review publication")?;
    case.start()?;
    assert_eq!(assert_final(&case, &pool, &run, 3, None).await?, bytes);
    assert_eq!(server.requests().await.len(), 1);
    case.event(json!({"event":"provider_requests","count":1}))?;
    let _ = release.send(());
    pool.close().await;
    server.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn candidate_crash_and_abort_supersession_recover_one_authoritative_report() -> Result<()> {
    for supersede in [false, true] {
        let server = responses::start_mock_server().await;
        provider(&server, "normal").await;
        let case = Case::new(
            if supersede {
                "supersession"
            } else {
                "normal-candidate"
            },
            &server.uri(),
        )?;
        let pool = initialise(&case).await?;
        gate(&pool, "publication").await?;
        let run = start_run(&case)?;
        let normal = candidate(&case, &pool, &run).await?;
        wait_state(&pool, &run, true).await?;
        snapshot(&case, &pool, &run, "normal_candidate_unpublished").await?;
        let expected = if supersede {
            abort_ack(&case, &run).await?;
            let aborted = candidate(&case, &pool, &run).await?;
            assert_ne!(normal.selection, aborted.selection);
            assert_ne!(normal.id, aborted.id);
            assert!(String::from_utf8_lossy(&aborted.bytes).contains("CLI fixture audit"));
            let state = snapshot(&case, &pool, &run, "abort_superseded_normal_candidate").await?;
            assert_eq!(state["generation"], 2);
            assert_eq!(state["attempts"][0][1], "succeeded");
            aborted
        } else {
            normal
        };
        case.crash(if supersede {
            "aborted candidate after normal supersession; publication gated"
        } else {
            "normal candidate exists; terminal publication gated"
        })?;
        // Restart with the gate intact first: selection/candidate identity must
        // survive while neither report is allowed to become authoritative.
        case.start()?;
        wait_state(&pool, &run, true).await?;
        let recovered = candidate(&case, &pool, &run).await?;
        assert_eq!(recovered.id, expected.id);
        assert_eq!(recovered.bytes, expected.bytes);
        retry_refused(&case, &pool, &run).await?;
        let mut follower = tokio::process::Command::from(audit(&case))
            .args(["read", &run, "--follow"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let mut lines = tokio::io::AsyncBufReadExt::lines(tokio::io::BufReader::new(
            follower
                .stderr
                .take()
                .context("publication follower stderr")?,
        ));
        tokio::time::timeout(Duration::from_secs(10), async {
            while let Some(line) = lines.next_line().await? {
                if line.contains(&run) && line.contains("reporting") {
                    return anyhow::Ok(());
                }
            }
            anyhow::bail!("read --follow did not observe pending publication")
        })
        .await??;
        assert!(follower.try_wait()?.is_none());
        case.event(json!({"event":"read_follow_pending","run":run}))?;
        ungate(&pool, "publication").await?;
        let code = if supersede { 6 } else { 0 };
        let followed =
            tokio::time::timeout(Duration::from_secs(30), follower.wait_with_output()).await??;
        check(&followed, code)?;
        assert_eq!(followed.stdout, expected.bytes);
        case.event(json!({"event":"read_follow_published","exit":code,"content_digest":digest(&followed.stdout)}))?;
        let bytes = assert_final(&case, &pool, &run, code, Some(&expected)).await?;
        // Normal publication winning first must remain normal after a later Abort.
        let before = snapshot(&case, &pool, &run, "before_terminal_abort_replay").await?;
        check(&audit(&case).args(["abort", &run]).output()?, code)?;
        same_state(
            snapshot(&case, &pool, &run, "after_terminal_abort_replay").await?,
            before,
        );
        case.crash("terminal publication committed")?;
        case.start()?;
        assert_eq!(
            assert_final(&case, &pool, &run, code, Some(&expected)).await?,
            bytes
        );
        assert_eq!(
            server.received_requests().await.context("requests")?.len(),
            2
        );
        case.event(json!({"event":"provider_requests","count":2}))?;
        pool.close().await;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn debugger_controls_frozen_selection_before_first_render_then_sigkill_recovers() -> Result<()>
{
    for aborted in [false, true] {
        let server = responses::start_mock_server().await;
        provider(&server, "normal").await;
        let mut case = Case::new(
            if aborted {
                "abort-freeze-before-render"
            } else {
                "freeze-before-render"
            },
            &server.uri(),
        )?;
        // Exact production source line is derived at runtime so test-only edits do
        // not silently move the debugger to a different persistence boundary.
        let source = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../app-server/src/audex/store/reporting.rs")
            .canonicalize()?;
        let text = std::fs::read_to_string(&source)?;
        let matching: Vec<_> = text
            .lines()
            .enumerate()
            .filter(|(_, line)| line.contains("let rendered = collection::render(input)?;"))
            .collect();
        assert_eq!(matching.len(), 1);
        let line = matching[0].0 + 1;
        let marker = case.evidence.join("freeze.marker");
        let release = case.evidence.join("freeze.release");
        let script = case.evidence.join("freeze.gdb");
        std::fs::write(
            &script,
            format!(
                r#"set pagination off
set confirm off
set debuginfod enabled off
set startup-with-shell off
set print thread-events off
handle SIGPIPE nostop noprint pass
handle SIGTERM nostop noprint pass
break {}:{}
commands
silent
bt 5
python
import os,time,gdb,sqlite3
connection=sqlite3.connect({},uri=True)
stopped=connection.execute("SELECT control_intent FROM audit_runs").fetchone()
connection.close()
if not {} or (stopped and stopped[0]=='cancel_requested'):
    open({},'w').write(str(gdb.selected_inferior().pid))
    while not os.path.exists({}): time.sleep(0.05)
end
continue
end
starti
python open({},'w').write(str(gdb.selected_inferior().pid))
continue
"#,
                source.display(),
                line,
                serde_json::to_string(&format!(
                    "file:{}?mode=ro",
                    case.home.path().join("audex/audit-state.sqlite3").display()
                ))?,
                if aborted { "True" } else { "False" },
                serde_json::to_string(&marker)?,
                serde_json::to_string(&release)?,
                serde_json::to_string(&case.evidence.join("debugger.pid"))?
            ),
        )?;
        let output = std::fs::File::create(case.evidence.join("debugger.log"))?;
        case.daemon.unmanaged = Some(
            std::process::Command::new("gdb")
                .args(["-q", "-nx", "-batch", "-x"])
                .arg(&script)
                .arg("--args")
                .arg(&case.codex)
                .args(["app-server", "--listen", "unix://"])
                .env("CODEX_HOME", case.home.path())
                .stdin(Stdio::null())
                .stdout(output.try_clone()?)
                .stderr(output)
                .spawn()?,
        );
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                if case
                    .command()
                    .args(["app-server", "daemon", "version"])
                    .output()?
                    .status
                    .success()
                {
                    return anyhow::Ok(());
                }
                if let Some(status) = case
                    .daemon
                    .unmanaged
                    .as_mut()
                    .context("debugger")?
                    .try_wait()?
                {
                    anyhow::bail!(
                        "debugger exited before daemon readiness: {status}; see debugger.log"
                    );
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await??;
        case.remember_pid()?;
        check(&audit(&case).arg("list").output()?, 0)?;
        let pool = db(&case).await?;
        gate(&pool, "publication").await?;
        let run = start_run(&case)?;
        let mut abort_client = if aborted {
            candidate(&case, &pool, &run).await?; // Normal candidate remains unpublished.
            Some(
                tokio::process::Command::from(audit(&case))
                    .args(["abort", &run])
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .kill_on_drop(true)
                    .spawn()?,
            )
        } else {
            None
        };
        tokio::time::timeout(Duration::from_secs(30), async {
            while !marker.exists() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await?;
        assert_eq!(
            std::fs::read_to_string(&marker)?.parse::<u32>()?,
            case.current_pid()?
        );
        wait_state(&pool, &run, true).await?;
        let state = snapshot(&case, &pool, &run, "debugger_before_collection_render").await?;
        let selected = state["selections"]
            .as_array()
            .context("selections")?
            .iter()
            .find(|entry| entry[0] == state["authoritative_selection"])
            .context("authoritative frozen selection")?;
        let input_json = selected[2].as_str().context("frozen JSON")?;
        let input: Value = serde_json::from_str(input_json)?;
        assert_eq!(
            input["outcome"],
            if aborted {
                "aborted_by_operator"
            } else {
                "success"
            }
        );
        assert_eq!(state["generation"], if aborted { 2 } else { 1 });
        let version = serde_json::from_str::<Value>(input_json)?["format_version"]
            .as_u64()
            .context("report version")?;
        assert_eq!(version, 5);
        let expected_id = format!(
            "audex-report-v{version}:sha256:{}",
            digest(format!("[{version},{input_json}]").as_bytes())
        );
        let reports = case.home.path().join("audex/reports").join(&run);
        let authoritative_path = reports.join(format!(
            "{}.md",
            digest(format!("[{version},{input_json}]").as_bytes())
        ));
        assert!(
            !authoritative_path.exists(),
            "the authoritative selection has not rendered a candidate"
        );
        if !aborted {
            assert!(!reports.exists() || std::fs::read_dir(&reports)?.next().is_none());
        }
        case.event(json!({"event":"debugger_boundary","source":source,"line":line,"expected_report_id":expected_id,"authoritative_candidate_exists":false,"previous_normal_candidate_exists":aborted}))?;
        case.crash(if aborted {
            "Abort supersession committed; debugger before aborted selection first render"
        } else {
            "debugger breakpoint before collection::render; normal freeze committed"
        })?;
        if let Some(client) = abort_client.take() {
            let output =
                tokio::time::timeout(Duration::from_secs(10), client.wait_with_output()).await??;
            check(&output, 1)?;
            case.event(json!({"event":"abort_frontend_lost_before_publication","exit":1,"ack_progress_observed":String::from_utf8_lossy(&output.stderr).contains("cancellation requested")}))?;
        }
        if let Some(mut debugger) = case.daemon.unmanaged.take() {
            debugger.wait()?;
        }
        ungate(&pool, "publication").await?;
        case.start()?;
        let code = if aborted { 6 } else { 0 };
        let bytes = assert_final(&case, &pool, &run, code, None).await?;
        let state = snapshot(&case, &pool, &run, "recovered_frozen_selection").await?;
        assert_eq!(state["current_report"], expected_id);
        case.crash("terminal publication after pre-render crash")?;
        case.start()?;
        assert_eq!(assert_final(&case, &pool, &run, code, None).await?, bytes);
        let terminal = snapshot(
            &case,
            &pool,
            &run,
            "before_replayed_stop_after_debugger_recovery",
        )
        .await?;
        check(&audit(&case).args(["abort", &run]).output()?, code)?;
        same_state(
            snapshot(
                &case,
                &pool,
                &run,
                "after_replayed_stop_after_debugger_recovery",
            )
            .await?,
            terminal,
        );
        assert_eq!(
            server.received_requests().await.context("requests")?.len(),
            2
        );
        case.event(json!({"event":"provider_requests","count":2}))?;
        pool.close().await;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn private_completed_and_partial_peers_survive_aborted_candidate_process_loss() -> Result<()>
{
    let server = responses::start_mock_server().await;
    provider(&server, "normal").await;
    let partial = "PRIVATE_PEER_PRE_ABORT_PARTIAL {";
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(wiremock::matchers::body_string_contains(
            "s1-private-assignment",
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(responses::sse(vec![
                    responses::ev_response_created("private-partial"),
                    responses::ev_message_item_added("private-partial", ""),
                    responses::ev_output_text_delta(partial),
                ])),
        )
        .with_priority(1)
        .expect(1)
        .mount(&server)
        .await;
    let case = Case::new("mixed-peers", &server.uri())?;
    let pool = initialise(&case).await?;
    gate(&pool, "commission").await?;
    gate(&pool, "publication").await?;
    let run = start_run(&case)?;
    // Private controlled fixture, before any reviewer dispatch or readiness.
    // Copy the established native input/policy binding, preserving isolation;
    // this does not expose public roster commissioning or edit a live roster.
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let (phase, ready): (String, bool) =
        sqlx::query_as("SELECT phase,collection_ready FROM audit_runs WHERE id=?")
            .bind(&run)
            .fetch_one(&mut *tx)
            .await?;
    assert_eq!(phase, "created");
    assert!(!ready);
    sqlx::query("INSERT INTO reviewer_assignments(id,run_id,ordinal,snapshot_id,provider,model,reasoning_effort,reviewer_input_digest,state,created_at_ms,updated_at_ms)
        SELECT 's1-private-assignment',run_id,1,snapshot_id,provider,model,reasoning_effort,reviewer_input_digest,'planned',created_at_ms,updated_at_ms FROM reviewer_assignments WHERE run_id=? AND ordinal=0")
        .bind(&run).execute(&mut *tx).await?;
    sqlx::query("INSERT INTO operations(id,assignment_id,kind,state,attempt,input_digest,created_at_ms,updated_at_ms,submission_boundary)
        SELECT 's1-private-operation','s1-private-assignment','review','planned',0,o.input_digest,o.created_at_ms,o.updated_at_ms,'unsubmitted'
        FROM operations o JOIN reviewer_assignments a ON a.id=o.assignment_id WHERE a.run_id=? AND a.ordinal=0")
        .bind(&run).execute(&mut *tx).await?;
    sqlx::query("INSERT INTO audit_reviewers(id,run_id,ordinal,current_assignment_id) VALUES('s1-private-reviewer',?,1,'s1-private-assignment')").bind(&run).execute(&mut *tx).await?;
    sqlx::query("INSERT INTO audit_attempts(assignment_id,reviewer_id,run_id,ordinal,policy_digest)
        SELECT 's1-private-assignment','s1-private-reviewer',run_id,1,policy_digest FROM audit_attempts WHERE run_id=? LIMIT 1").bind(&run).execute(&mut *tx).await?;
    sqlx::query("INSERT INTO audit_attempt_inputs(assignment_id,request_json,input_digest,policy_json,policy_digest)
        SELECT 's1-private-assignment',i.request_json,i.input_digest,i.policy_json,i.policy_digest FROM audit_attempt_inputs i JOIN reviewer_assignments a ON a.id=i.assignment_id WHERE a.run_id=? AND a.ordinal=0")
        .bind(&run).execute(&mut *tx).await?;
    tx.commit().await?;
    assert!(
        server
            .received_requests()
            .await
            .context("requests")?
            .is_empty()
    );
    case.crash("private two-reviewer fixture prepared before dispatch")?;
    ungate(&pool, "commission").await?;
    case.start()?;
    tokio::time::timeout(Duration::from_secs(30),async {
        loop {
            let (completed,text):(i64,String)=sqlx::query_as("SELECT (SELECT count(*) FROM reviewer_assignments WHERE run_id=? AND state='succeeded'),COALESCE((SELECT group_concat(payload,'') FROM audit_activity WHERE assignment_id='s1-private-assignment' AND kind IN ('assistant_delta','assistant_message')),'')")
                .bind(&run).fetch_one(&pool).await?;
            if completed==1 && text==partial { return anyhow::Ok(()); }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }).await??;
    let before = snapshot(&case, &pool, &run, "one_completed_one_partial").await?;
    assert_eq!(before["ready"], false);
    assert_eq!(before["phase"], "reviewing");
    abort_ack(&case, &run).await?;
    let expected = candidate(&case, &pool, &run).await?;
    let text = String::from_utf8_lossy(&expected.bytes);
    assert!(text.contains("CLI fixture audit") && text.contains(partial));
    assert!(text.find("CLI fixture audit") < text.find(partial));
    snapshot(&case, &pool, &run, "mixed_aborted_candidate_before_kill").await?;
    case.crash(
        "completed peer plus eligible partial in aborted candidate; terminal publication gated",
    )?;
    ungate(&pool, "publication").await?;
    case.start()?;
    assert_final(&case, &pool, &run, 6, Some(&expected)).await?;
    let after = snapshot(&case, &pool, &run, "mixed_after_recovery").await?;
    assert_eq!(
        after["attempts"][0][4], before["attempts"][0][4],
        "completed result digest retained"
    );
    let output = audit(&case)
        .args(["attempt", "read", &run, "s1-private-assignment"])
        .output()?;
    check(&output, 0)?;
    let read: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(super::attempt_controls::assistant_text(&read), partial);
    assert_eq!(read["attempt"]["completed"], false);
    assert_eq!(
        server.received_requests().await.context("requests")?.len(),
        3
    );
    case.event(json!({"event":"provider_requests","count":3}))?;
    pool.close().await;
    Ok(())
}

fn same_state(mut first: Value, mut second: Value) {
    first.as_object_mut().expect("state").remove("label");
    second.as_object_mut().expect("state").remove("label");
    assert_eq!(first, second);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn actual_terminal_snapshot_failure_exits_two_without_provider_inference() -> Result<()> {
    let server = responses::start_mock_server().await;
    let case = Case::new("terminal-failure", &server.uri())?;
    let pool = initialise(&case).await?;
    gate(&pool, "commission").await?;
    let run = start_run(&case)?;
    case.crash("owned snapshot captured; reviewer dispatch gated")?;
    let relative: String = sqlx::query_scalar("SELECT manifest_path FROM audit_runs WHERE id=?")
        .bind(&run)
        .fetch_one(&pool)
        .await?;
    let manifest = case.home.path().join("audex").join(relative);
    // Deliberate corruption of this fixture's sealed source, never a real audit.
    std::fs::set_permissions(&manifest, std::fs::Permissions::from_mode(0o600))?;
    std::fs::write(&manifest, b"fixture corrupt manifest")?;
    ungate(&pool, "commission").await?;
    case.start()?;
    let output = tokio::time::timeout(
        Duration::from_secs(15),
        tokio::process::Command::from(audit(&case))
            .args(["read", &run, "--follow"])
            .kill_on_drop(true)
            .output(),
    )
    .await??;
    check(&output, 2)?;
    assert!(output.stdout.is_empty());
    let state = snapshot(&case, &pool, &run, "actual_terminal_failure").await?;
    assert_eq!(state["outcome"], "snapshot_integrity_failure");
    assert!(state["current_report"].is_null());
    assert!(
        server
            .received_requests()
            .await
            .context("requests")?
            .is_empty()
    );
    case.event(json!({"event":"final_cli","exit":2,"provider_requests":0}))?;
    pool.close().await;
    Ok(())
}
