//! Real CLI + managed daemon + real Codex loop; provider HTTP stays on loopback.
#![cfg(unix)]

#[path = "app_server_daemon/support.rs"]
mod support;

#[path = "audit/follow_progress.rs"]
mod follow_progress;

#[path = "audit/attempt_controls.rs"]
mod attempt_controls;

use anyhow::{Context, Result, ensure};
use core_test_support::responses;
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::process::{Output, Stdio};
use std::time::Duration;
use support::{TestDaemon, signal, wait_for_exit};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

fn configured(server: &MockServer) -> Result<TestDaemon> {
    configured_url(&server.uri())
}

fn configured_url(url: &str) -> Result<TestDaemon> {
    // A caller can choose a roomy directory on the build filesystem to permit
    // hard links instead of copying a large debug CLI into a small /tmp tmpfs.
    let homes = std::env::var_os("AUDEX_TEST_TMPDIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    std::fs::create_dir_all(&homes)?;
    let daemon = TestDaemon::new_in(&homes)?;
    ensure!(
        codex_git_utils::get_git_repo_root(daemon.home.path()).is_none(),
        "set AUDEX_TEST_TMPDIR to a directory outside a Git worktree, so ordinary Codex Git metadata discovery cannot affect this version-control-neutral fixture"
    );
    std::fs::write(
        daemon.home.path().join("app-server-daemon/settings.json"),
        r#"{"shutdownGraceSeconds":0,"updater":{"autoUpdateEnabled":false}}"#,
    )?;
    std::fs::write(
        daemon.home.path().join("config.toml"),
        format!(
            r#"
model = "audit-test"
model_provider = "audit_fixture"
developer_instructions = "HOST_DEVELOPER_FORBIDDEN"
[features]
plugins = false
remote_plugin = false
plugin_discovery_prefetch = false
[model_providers.audit_fixture]
name = "Local audit fixture"
base_url = "{}/v1"
wire_api = "responses"
request_max_retries = 0
stream_max_retries = 0
"#,
            url
        ),
    )?;
    std::fs::write(
        daemon.home.path().join("AGENTS.md"),
        "AMBIENT_AGENTS_FORBIDDEN",
    )?;
    Ok(daemon)
}

fn audit(daemon: &TestDaemon) -> std::process::Command {
    let mut command = daemon.command();
    command
        .arg("audit")
        .env_remove("OPENAI_API_KEY")
        .env_remove("CODEX_API_KEY")
        .env_remove("CODEX_ACCESS_TOKEN")
        .env_remove("CODEX_REMOTE")
        .env_remove("HTTP_PROXY")
        .env_remove("HTTPS_PROXY")
        .env_remove("ALL_PROXY");
    command
}

fn check(output: &Output, code: i32) -> Result<()> {
    ensure!(
        output.status.code() == Some(code),
        "expected {code}: status={}\nstdout={}\nstderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

#[expect(
    clippy::expect_used,
    reason = "Wiremock responders cannot return Result; malformed fixture requests must fail the test"
)]
async fn provider(server: &MockServer, mode: &'static str) {
    Mock::given(method("POST")).and(path("/v1/responses")).respond_with(move |request: &wiremock::Request| {
        let body:Value=serde_json::from_slice(&request.body).expect("provider request JSON");
        let text=body.to_string();
        assert!(!text.contains("HOST_DEVELOPER_FORBIDDEN"));
        assert!(!text.contains("AMBIENT_AGENTS_FORBIDDEN"));
        assert_eq!(body["model"],"audit-test");
        assert_eq!(body["tools"].as_array().expect("tools").len(),1);
        assert_eq!(body["tools"][0]["name"],"audex_source");
        assert_eq!(body["tools"][0]["tools"].as_array().expect("source tools").len(),3);
        let input=body["input"].as_array().expect("input");
        let assignment=input.iter().filter_map(|item|item["content"].as_array()).flatten()
            .filter_map(|part|part["text"].as_str()).filter_map(|text|serde_json::from_str::<Value>(text).ok())
            .find(|value|value.get("assignment_id").is_some()).expect("assignment");
        let output=if mode=="bad" {
            responses::sse(vec![responses::ev_assistant_message("answer","not valid audit JSON"),responses::ev_completed("bad")])
        } else if mode=="blocked" {
            responses::sse(vec![responses::ev_completed("blocked")])
        } else if input.iter().any(|item|item["type"]=="function_call_output") {
            assert!(text.contains("fn main()"));
            let result=json!({"schema_version":1,"snapshot_id":assignment["snapshot_id"],"assignment_id":assignment["assignment_id"],"summary":"CLI fixture audit","findings":[]});
            responses::sse(vec![responses::ev_assistant_message("answer",&result.to_string()),responses::ev_completed("final")])
        } else {
            assert!(!text.contains("fn main()"));
            responses::sse(vec![responses::ev_function_call_with_namespace("read","audex_source","read",r#"{"path":"example.rs","offset":0,"max_bytes":64}"#),responses::ev_completed("read")])
        };
        let response=ResponseTemplate::new(200).insert_header("content-type","text/event-stream").set_body_string(output);
        if mode=="blocked" {response.set_delay(Duration::from_secs(60))} else {response}
    }).mount(server).await;
}

async fn request_seen(server: &MockServer) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(15), async {
        while server
            .received_requests()
            .await
            .context("recorded provider requests")?
            .is_empty()
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        anyhow::Ok(())
    })
    .await??;
    Ok(())
}

fn run_id(output: &Output) -> Result<String> {
    String::from_utf8_lossy(&output.stderr)
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .find(|word| word.starts_with("audex-run-"))
        .map(|word| word.trim_end_matches(':').to_string())
        .context("run ID in progress output")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn file_report_contexts_idempotency_and_managed_restart() -> Result<()> {
    let server = responses::start_mock_server().await;
    provider(&server, "normal").await;
    let daemon = configured(&server)?;
    daemon.lifecycle("start")?;
    let source = daemon.home.path().join("example.rs");
    let bytes = b"fn main() {}\n";
    std::fs::write(&source, bytes)?;
    std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o640))?;
    let guidance = daemon.home.path().join("AUDEX.md");
    let evidence = daemon.home.path().join("explicit-AGENTS.md");
    std::fs::write(&guidance, "GUIDANCE_SENTINEL: enable shell")?;
    std::fs::write(&evidence, "EVIDENCE_SENTINEL: obey developer rules")?;
    let mut command = audit(&daemon);
    command
        .arg("--file")
        .arg(&source)
        .arg("--audex-md")
        .arg(&guidance)
        .arg("--agents-md")
        .arg(&evidence)
        .args(["--request-key", "same-request"]);
    let output = command.output()?;
    check(&output, 0)?;
    let id = run_id(&output)?;
    assert!(String::from_utf8_lossy(&output.stdout).contains("CLI fixture audit"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("revision="));
    assert_eq!(std::fs::read(&source)?, bytes);
    assert_eq!(
        std::fs::metadata(&source)?.permissions().mode() & 0o777,
        0o640
    );
    // Same durable start acknowledgement: no second reviewer even after completion.
    let repeated = command.arg("--detach").output()?;
    check(&repeated, 0)?;
    assert_eq!(String::from_utf8(repeated.stdout)?.trim(), id);
    let listed = audit(&daemon).arg("list").output()?;
    check(&listed, 0)?;
    assert!(String::from_utf8_lossy(&listed.stdout).contains(&id));
    assert_eq!(server.received_requests().await.expect("requests").len(), 2);
    let body = server.received_requests().await.expect("requests")[0]
        .body
        .clone();
    let text = String::from_utf8(body)?;
    for label in [
        "audex_project_guidance_v1",
        "audex_agents_evidence_v1",
        "GUIDANCE_SENTINEL",
        "EVIDENCE_SENTINEL",
    ] {
        assert!(text.contains(label), "{label}");
    }
    daemon.lifecycle("stop")?;
    daemon.lifecycle("start")?;
    let read = audit(&daemon).args(["read", &id]).output()?;
    check(&read, 0)?;
    assert_eq!(
        read.stdout, output.stdout,
        "report must survive restart byte for byte"
    );
    assert_eq!(server.received_requests().await.expect("requests").len(), 2);
    assert!(
        !daemon
            .home
            .path()
            .join("app-server-daemon/app-server-updater.pid")
            .exists()
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stdin_disconnect_cancellation_and_failure_have_distinct_statuses() -> Result<()> {
    for mode in ["blocked", "bad"] {
        let server = responses::start_mock_server().await;
        provider(&server, mode).await;
        let daemon = configured(&server)?;
        daemon.lifecycle("start")?;
        let mut child = audit(&daemon)
            .args([
                "--stdin",
                "--name",
                "example.rs",
                "--model",
                "audit-test",
                "--provider",
                "audit_fixture",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        child
            .stdin
            .take()
            .context("stdin")?
            .write_all(b"fn main() {}\n")?;
        request_seen(&server).await?;
        if mode == "bad" {
            check(&child.wait_with_output()?, 2)?;
        } else {
            // SIGINT is a client detach, never an implicit audit/cancel.
            signal(child.id(), libc::SIGINT)?;
            let disconnected = child.wait_with_output()?;
            check(&disconnected, 130)?;
            // Listing also recovers the identity if the client lost its start
            // acknowledgement before it could print a progress line.
            let listed = audit(&daemon).arg("list").output()?;
            check(&listed, 0)?;
            let id = String::from_utf8(listed.stdout)?
                .split_whitespace()
                .next()
                .context("listed run")?
                .to_string();
            let read = audit(&daemon).args(["read", &id]).output()?;
            check(&read, 5)?;
            assert!(!String::from_utf8_lossy(&read.stderr).contains("cancellation requested"));
            assert_no_children(daemon.pid("app-server.pid")?)?;
            let cancelled = audit(&daemon).args(["cancel", &id]).output()?;
            check(&cancelled, 3)?;
            assert!(String::from_utf8_lossy(&cancelled.stderr).contains("cancellation requested"));
            let later = audit(&daemon).args(["read", &id]).output()?;
            check(&later, 3)?;
        }
        assert_eq!(server.received_requests().await.expect("requests").len(), 1);
    }
    Ok(())
}

fn assert_no_children(pid: u32) -> Result<()> {
    #[cfg(target_os = "linux")]
    for entry in std::fs::read_dir(format!("/proc/{pid}/task"))? {
        let children = std::fs::read_to_string(entry?.path().join("children"))?;
        ensure!(
            children.trim().is_empty(),
            "unexpected daemon child: {children}; details={:?}",
            children
                .split_whitespace()
                .map(|child| (
                    std::fs::read_link(format!("/proc/{child}/exe")),
                    std::fs::read_link(format!("/proc/{child}/cwd")),
                    std::fs::read(format!("/proc/{child}/cmdline"))
                        .map(|bytes| String::from_utf8_lossy(&bytes).replace('\0', " "))
                ))
                .collect::<Vec<_>>()
        );
    }
    #[cfg(not(target_os = "linux"))]
    let _ = pid;
    Ok(())
}

// Fault injection belongs only to the isolated test fixture, never the CLI.
#[expect(
    clippy::disallowed_methods,
    reason = "test-only access to the existing Audex fixture DB must preserve its FULL/MEMORY connection settings; codex-state's shim fixes synchronous to NORMAL"
)]
async fn fixture_db(daemon: &TestDaemon) -> Result<sqlx::SqlitePool> {
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(daemon.home.path().join("audex/audit-state.sqlite3"))
        .create_if_missing(false)
        .foreign_keys(true)
        .busy_timeout(Duration::from_secs(5))
        .pragma("synchronous", "FULL")
        .pragma("temp_store", "MEMORY")
        .pragma("journal_mode", "WAL")
        .pragma("auto_vacuum", "INCREMENTAL");
    Ok(sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await?)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cold_pending_and_terminal_unknown_are_exposed_without_resampling() -> Result<()> {
    let server = responses::start_mock_server().await;
    provider(&server, "blocked").await;
    let daemon = configured(&server)?;
    daemon.lifecycle("start")?;
    let mut child = audit(&daemon)
        .args(["--stdin", "--name", "example.rs", "--detach"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .context("stdin")?
        .write_all(b"fn main() {}\n")?;
    let submitted = child.wait_with_output()?;
    check(&submitted, 0)?;
    let id = String::from_utf8(submitted.stdout)?.trim().to_string();
    request_seen(&server).await?;
    let pid = daemon.pid("app-server.pid")?;
    signal(pid, libc::SIGKILL)?;
    wait_for_exit(pid)?;
    let pool = fixture_db(&daemon).await?;
    // Deterministically advance lease expiry after the process has died.
    sqlx::query("UPDATE operations SET lease_expires_at_ms=0")
        .execute(&pool)
        .await?;
    pool.close().await;
    daemon.lifecycle("start")?;
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let output = audit(&daemon).args(["read", &id]).output()?;
            check(&output, 5)?;
            if String::from_utf8_lossy(&output.stderr).contains("observation pending") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        anyhow::Ok(())
    })
    .await??;
    assert_eq!(server.received_requests().await.expect("requests").len(), 1);
    daemon.lifecycle("stop")?;
    // An explicit retained terminal unknown fixture exercises exit 4, separately
    // from the accepted non-terminal observation-only policy above.
    let pool = fixture_db(&daemon).await?;
    let mut tx = pool.begin().await?;
    sqlx::query("UPDATE operations SET state='unknown',error='fixture unknown',retry_classification='never_automatically_retry',lease_owner=NULL,lease_expires_at_ms=NULL").execute(&mut *tx).await?;
    sqlx::query("UPDATE reviewer_assignments SET state='unknown',failure_code='fixture unknown'")
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "UPDATE audit_runs SET phase='failed',outcome='fixture unknown',revision=revision+1",
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    pool.close().await;
    daemon.lifecycle("start")?;
    let unknown = audit(&daemon).args(["read", &id, "--follow"]).output()?;
    check(&unknown, 4)?;
    assert!(String::from_utf8_lossy(&unknown.stderr).contains("unknown"));
    assert_eq!(server.received_requests().await.expect("requests").len(), 1);
    Ok(())
}

#[tokio::test]
async fn missing_daemon_is_a_local_error_without_service_start() -> Result<()> {
    let server = responses::start_mock_server().await;
    let daemon = configured(&server)?;
    let missing = audit(&daemon).arg("list").output()?;
    check(&missing, 1)?;
    assert!(String::from_utf8_lossy(&missing.stderr).contains("app-server daemon start"));
    assert!(
        !daemon
            .home
            .path()
            .join("app-server-daemon/app-server.pid")
            .exists()
    );
    assert!(
        server
            .received_requests()
            .await
            .expect("requests")
            .is_empty()
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_capacity_release_after_runtime_loss_allows_another_audit_without_replay()
-> Result<()> {
    let server = responses::start_mock_server().await;
    provider(&server, "blocked").await;
    let daemon = configured(&server)?;
    let source = daemon.home.path().join("first.rs");
    std::fs::write(&source, "fn main() {}\n")?;
    daemon.lifecycle("start")?;
    let submitted = audit(&daemon)
        .arg("--file")
        .arg(&source)
        .arg("--detach")
        .output()?;
    check(&submitted, 0)?;
    let first = String::from_utf8(submitted.stdout)?.trim().to_owned();
    request_seen(&server).await?;
    let pid = daemon.pid("app-server.pid")?;
    signal(pid, libc::SIGKILL)?;
    wait_for_exit(pid)?;
    let pool = fixture_db(&daemon).await?;
    sqlx::query("UPDATE operations SET lease_expires_at_ms=0")
        .execute(&pool)
        .await?;
    pool.close().await;
    daemon.lifecycle("start")?;
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let output = audit(&daemon).args(["read", &first]).output()?;
            check(&output, 5)?;
            if String::from_utf8_lossy(&output.stderr).contains("observation pending") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        anyhow::Ok(())
    })
    .await??;
    // Cancellation follows too: pending observation must not drop the watch.
    let mut cancellation = audit(&daemon)
        .args(["cancel", &first])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let read = audit(&daemon).args(["read", &first]).output()?;
            check(&read, 5)?;
            if String::from_utf8_lossy(&read.stderr).contains("cancellation requested") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        anyhow::Ok(())
    })
    .await??;
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert!(
        cancellation.try_wait()?.is_none(),
        "pending cancellation stays attached"
    );
    signal(cancellation.id(), libc::SIGINT)?;
    check(&cancellation.wait_with_output()?, 130)?;
    let second_source = daemon.home.path().join("second.rs");
    std::fs::write(&second_source, "fn independent() {}\n")?;
    let second = audit(&daemon)
        .arg("--file")
        .arg(&second_source)
        .arg("--detach")
        .output()?;
    check(&second, 0)?;
    let second = String::from_utf8(second.stdout)?.trim().to_owned();
    check(&audit(&daemon).args(["read", &second]).output()?, 5)?;
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    check(
        &audit(&daemon)
            .args(["release-slot", &first, "--reason", "Accept overlap"])
            .output()?,
        2,
    )?;
    for _ in 0..2 {
        let output = audit(&daemon)
            .args([
                "release-slot",
                &first,
                "--acknowledge-concurrent-execution-and-charges",
                "--reason",
                "Accept overlap",
            ])
            .output()?;
        check(&output, 5)?;
        assert!(String::from_utf8_lossy(&output.stderr).contains("capacity released by operator"));
    }
    tokio::time::timeout(Duration::from_secs(15), async {
        while server.received_requests().await.unwrap().len() < 2 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?;
    let old = audit(&daemon).args(["read", &first]).output()?;
    check(&old, 5)?;
    assert!(String::from_utf8_lossy(&old.stderr).contains("observation pending"));
    assert!(String::from_utf8_lossy(&old.stderr).contains("capacity released by operator"));
    check(&audit(&daemon).args(["cancel", &second]).output()?, 3)?;
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        2,
        "one sample per independent audit; original is never replayed"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn incompatible_protocol_and_missing_selection_fail_before_submission() -> Result<()> {
    use codex_app_server_protocol::AUDIT_PROTOCOL_VERSION;
    use codex_app_server_transport::{app_server_control_socket_path, bind_private_daemon_socket};
    use tokio_tungstenite::tungstenite::Message;

    for (version, config, flags, expected_error) in [
        (
            None,
            json!({}),
            vec!["--model", "audit-test", "--provider", "audit_fixture"],
            "compatibility check failed",
        ),
        (
            Some(AUDIT_PROTOCOL_VERSION + 1),
            json!({}),
            vec!["--model", "audit-test", "--provider", "audit_fixture"],
            "incompatible Audex daemon protocol",
        ),
        (
            Some(AUDIT_PROTOCOL_VERSION),
            json!({"model_provider":"audit_fixture"}),
            vec![],
            "select --model",
        ),
        (
            Some(AUDIT_PROTOCOL_VERSION),
            json!({"model":"audit-test"}),
            vec![],
            "select --provider",
        ),
        (
            Some(AUDIT_PROTOCOL_VERSION),
            json!({}),
            vec!["--model", "audit-test"],
            "select --provider",
        ),
        (
            Some(AUDIT_PROTOCOL_VERSION),
            json!({}),
            vec!["--provider", "audit_fixture"],
            "select --model",
        ),
    ] {
        let provider = responses::start_mock_server().await;
        let daemon = configured(&provider)?;
        let source = daemon.home.path().join("example.rs");
        std::fs::write(&source, b"fn main() {}\n")?;
        let socket = app_server_control_socket_path(daemon.home.path())?;
        // Reuse Codex's private socket binding and the normal client handshake.
        // The tiny test peer represents an older daemon; it cannot start a run.
        let (mut listener, guard) = bind_private_daemon_socket(socket.as_path()).await?;
        let peer = tokio::spawn(async move {
            let _guard = guard;
            let stream = listener.accept().await?;
            let mut stream = tokio_tungstenite::accept_async(stream).await?;
            let mut methods = Vec::new();
            while let Some(message) = stream.next().await {
                match message? {
                    Message::Text(text) => {
                        let request: Value = serde_json::from_str(&text)?;
                        if request.get("id").is_none() {
                            continue;
                        }
                        let method = request["method"].as_str().context("RPC method")?;
                        methods.push(method.to_string());
                        let result = match method {
                            "initialize" => json!({"userAgent":"codex/0.0.0"}),
                            "audit/capabilities" => {
                                let mut list = json!({"runs":[],"nextCursor":null});
                                if let Some(version) = version {
                                    list["protocolVersion"] = json!(version);
                                }
                                list
                            }
                            "config/read" => json!({"config":config,"origins":{}}),
                            _ => anyhow::bail!("CLI crossed preflight boundary with {method}"),
                        };
                        stream
                            .send(Message::Text(
                                json!({"jsonrpc":"2.0","id":request["id"],"result":result})
                                    .to_string()
                                    .into(),
                            ))
                            .await?;
                    }
                    Message::Close(_) => {
                        // Codex's client closes its socket after sending Close.
                        // All RPC responses above are already flushed; teardown
                        // is not another request and must not hide that evidence.
                        break;
                    }
                    _ => {}
                }
            }
            anyhow::Ok(methods)
        });
        let output = audit(&daemon)
            .arg("--file")
            .arg(&source)
            .arg("--detach")
            .args(flags)
            .output()?;
        check(&output, 1)?;
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(expected_error),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let methods = tokio::time::timeout(Duration::from_secs(10), peer).await???;
        assert_eq!(
            methods,
            if version == Some(AUDIT_PROTOCOL_VERSION) {
                vec!["initialize", "audit/capabilities", "config/read"]
            } else {
                vec!["initialize", "audit/capabilities"]
            }
        );
        assert!(
            !daemon.home.path().join("audex").exists(),
            "no snapshot or run may be created by preflight"
        );
        assert!(
            provider
                .received_requests()
                .await
                .context("requests")?
                .is_empty()
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn attached_clients_follow_pending_recovery_while_nonfollowing_reads_return_pending()
-> Result<()> {
    use codex_app_server_transport::{app_server_control_socket_path, bind_private_daemon_socket};
    use tokio_tungstenite::tungstenite::Message;

    for mode in ["read", "follow", "attached", "cancel"] {
        let provider = responses::start_mock_server().await;
        let daemon = configured(&provider)?;
        let source = daemon.home.path().join("example.rs");
        std::fs::write(&source, b"fn main() {}\n")?;
        let socket = app_server_control_socket_path(daemon.home.path())?;
        let (mut listener, guard) = bind_private_daemon_socket(socket.as_path()).await?;
        let peer = tokio::spawn(async move {
            let _guard = guard;
            let mut stream = tokio_tungstenite::accept_async(listener.accept().await?).await?;
            let mut reads = 0;
            let mut starts = 0;
            while let Some(message) = stream.next().await {
                let Message::Text(text) = message? else {
                    break;
                };
                let request: Value = serde_json::from_str(&text)?;
                if request.get("id").is_none() {
                    continue;
                }
                let method = request["method"].as_str().context("method")?;
                let result = match method {
                    "initialize" => json!({"userAgent":"codex/0.0.0"}),
                    "audit/capabilities" => {
                        json!({"protocolVersion":codex_app_server_protocol::AUDIT_PROTOCOL_VERSION})
                    }
                    "audit/start" | "audit/read" | "audit/cancel" => {
                        if method == "audit/start" {
                            starts += 1;
                        }
                        reads += 1;
                        let complete = reads >= 4;
                        json!({"run":{
                            "runId":"retained-run","snapshotId":"snapshot","assignmentId":"assignment",
                            "revision":if complete {3} else if reads >= 3 {2} else {1},
                            "phase":if complete {if mode == "cancel" {"cancelled"} else {"completed"}} else {"reviewing"},
                            "operationState":if complete {if mode == "cancel" {"failed"} else {"committed"}} else {"accepted"},
                            "observationPending":reads < 3,"capacityReleased":false,
                            "cancellationRequested":mode == "cancel","outcome":null,"rawResult":null,
                            "report":if complete && mode != "cancel" {Some("# Recovered report\n")} else {None}
                        }})
                    }
                    _ => anyhow::bail!("unexpected client request: {method}"),
                };
                stream
                    .send(Message::Text(
                        json!({"jsonrpc":"2.0","id":request["id"],"result":result})
                            .to_string()
                            .into(),
                    ))
                    .await?;
                if (2..4).contains(&reads) {
                    // Duplicate hints must not create a busy read loop or reset
                    // the fallback deadline on an unchanged pending revision.
                    stream
                        .send(Message::Text(
                            json!({"jsonrpc":"2.0","method":"audit/progress",
                        "params":{"runId":"retained-run","revision":1,"phase":"reviewing"}})
                            .to_string()
                            .into(),
                        ))
                        .await?;
                }
            }
            anyhow::Ok((reads, starts))
        });
        let mut command = audit(&daemon);
        match mode {
            "attached" => {
                command.arg("--file").arg(&source).args([
                    "--model",
                    "audit-test",
                    "--provider",
                    "audit_fixture",
                ]);
            }
            "read" => {
                command.args(["read", "retained-run"]);
            }
            "cancel" => {
                command.args(["cancel", "retained-run"]);
            }
            _ => {
                command.args(["read", "retained-run", "--follow"]);
            }
        }
        let began = std::time::Instant::now();
        let output = tokio::time::timeout(
            Duration::from_secs(15),
            tokio::task::spawn_blocking(move || command.output()),
        )
        .await???;
        check(
            &output,
            if mode == "read" {
                5
            } else if mode == "cancel" {
                3
            } else {
                0
            },
        )?;
        if mode != "read" {
            assert!(
                began.elapsed() >= Duration::from_secs(5),
                "unchanged hints must not force repeated reads"
            );
        }
        assert!(String::from_utf8_lossy(&output.stderr).contains("observation pending"));
        if mode == "follow" || mode == "attached" {
            assert_eq!(output.stdout, b"# Recovered report\n");
        }
        let (reads, starts) = tokio::time::timeout(Duration::from_secs(5), peer).await???;
        assert_eq!(reads, if mode == "read" { 1 } else { 4 });
        assert_eq!(starts, usize::from(mode == "attached"));
        assert!(
            provider
                .received_requests()
                .await
                .context("requests")?
                .is_empty()
        );
    }
    Ok(())
}
