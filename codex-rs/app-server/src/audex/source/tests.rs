use super::*;
use crate::audex::snapshot::{
    CaptureLimits, CaptureRequest, CaptureSource, ContextDocuments, SnapshotStore,
    manifest::sha256_hex,
};
use crate::audex::source_mcp::SourceMcpServer;
use crate::audex::store::attempts::{AttemptAction, ControlRequest};
use crate::audex::store::{
    AuditStore, CreateRunRequest, admission::AdmissionIdentity, source::ReadOnlySource,
};
use crate::audex::tools::SourceTools;
use codex_exec_server::LocalFileSystem;
use codex_extension_api::{ExtensionData, ToolContributor};
use codex_tools::{
    ConversationHistory, NoopTurnItemEmitter, ToolCall, ToolCallSource, ToolPayload,
};
use codex_utils_absolute_path::AbsolutePathBuf;
use rmcp::ServiceExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

struct Fixture {
    temp: tempfile::TempDir,
    root: AbsolutePathBuf,
    store: AuditStore,
    snapshot: SnapshotHandle,
    identity: AdmissionIdentity,
    manifest: String,
}

impl Fixture {
    async fn new(bytes: &[u8]) -> anyhow::Result<Self> {
        let temp = tempfile::tempdir()?;
        let root = AbsolutePathBuf::from_absolute_path(temp.path())?;
        let snapshot = SnapshotStore::new(
            root.join("snapshots"),
            Arc::new(LocalFileSystem::unsandboxed()),
            CaptureLimits::default(),
        )
        .capture(CaptureRequest {
            source: CaptureSource::Snippet {
                logical_path: SnapshotPath::new("example.rs")?,
                bytes: bytes.to_vec(),
            },
            context_documents: ContextDocuments::default(),
        })
        .await?;
        let manifest = sha256_hex(&snapshot.manifest().to_pretty_bytes()?);
        let store = AuditStore::open(root.clone()).await?;
        let policy = sha256_hex(b"{}");
        let run = store
            .create_run_with_input(
                &CreateRunRequest {
                    request_key: "source-fixture".into(),
                    snapshot_id: snapshot.snapshot_id().to_string(),
                    manifest_digest: manifest.clone(),
                    manifest_path: format!(
                        "snapshots/{}/manifest.json",
                        snapshot.snapshot_id().digest()
                    ),
                    brief_digest: sha256_hex(b"brief"),
                    policy_digest: policy.clone(),
                    input_digest: sha256_hex(b"{}"),
                    provider: "fixture".into(),
                    model: "fixture".into(),
                    reasoning_effort: None,
                    reviewer_input_digest: sha256_hex(b"reviewer"),
                    operation_input_digest: sha256_hex(b"{}"),
                },
                Some(("{}", "{}", &sha256_hex(b"source-identity"))),
            )
            .await?;
        let revision = store.bind_snapshot(&run.run_id, run.revision).await?;
        let now = chrono::Utc::now().timestamp_millis();
        store
            .claim_operation(&run.operation_id, "source-owner", now, now + 3_600_000)
            .await?;
        store
            .record_dispatch(&run.operation_id, "source-owner", revision, now)
            .await?;
        let execution = uuid::Uuid::now_v7().to_string();
        assert!(
            store
                .bind_execution(&run.operation_id, &execution, &policy, "source-owner", now)
                .await?
        );
        let attempt = store.run_for_operation(&run.operation_id).await?;
        let identity = AdmissionIdentity {
            run_id: run.run_id,
            assignment_id: run.assignment_id,
            operation_id: run.operation_id,
            execution_id: execution,
            owner: "source-owner".into(),
            owner_generation: attempt.owner_generation,
            run_stop_generation: store.run_stop_generation(&attempt.run_id).await?,
        };
        Ok(Self {
            temp,
            root,
            store,
            snapshot,
            identity,
            manifest,
        })
    }

    async fn authority(&self) -> anyhow::Result<Arc<ReadOnlySource>> {
        ReadOnlySource::open(
            &self.root,
            self.identity.clone(),
            self.snapshot.snapshot_id().to_string(),
            self.manifest.clone(),
        )
        .await
    }

    async fn stop(&self, action: AttemptAction) -> anyhow::Result<()> {
        self.store
            .control_attempt(
                &ControlRequest {
                    run_id: self.identity.run_id.clone(),
                    assignment_id: self.identity.assignment_id.clone(),
                    action_key: uuid::Uuid::now_v7().to_string(),
                    action: action.clone(),
                    reason: "source revocation fixture".into(),
                    authorise_retry: action == AttemptAction::Retry,
                    acknowledge_possible_overlap: action == AttemptAction::Retry,
                },
                None,
            )
            .await?;
        Ok(())
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fn writable_dirs(path: &Path) {
            if !std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir()) {
                return;
            }
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
            if let Ok(entries) = std::fs::read_dir(path) {
                for entry in entries.flatten() {
                    writable_dirs(&entry.path());
                }
            }
        }
        writable_dirs(self.temp.path());
    }
}

async fn wrapper(
    snapshot: &SnapshotHandle,
    plain: bool,
    name: &str,
    args: &str,
) -> Result<Value, String> {
    let contributor =
        SourceTools::with_plain_functions(snapshot, plain).map_err(|e| e.to_string())?;
    let scope = ExtensionData::new("source-parity");
    let tools = contributor.tools(&scope, &scope);
    let tool = &tools[TOOL_NAMES
        .iter()
        .position(|n| *n == name)
        .expect("fixture tool")];
    let expected = if plain {
        codex_tools::ToolName::plain(format!("audex_source__{name}"))
    } else {
        codex_tools::ToolName::namespaced("audex_source", name)
    };
    assert_eq!(tool.tool_name(), expected);
    match tool.spec() {
        codex_tools::ToolSpec::Function(spec) if plain => {
            assert_eq!(spec.name, format!("audex_source__{name}"))
        }
        codex_tools::ToolSpec::Namespace(spec) if !plain => assert_eq!(spec.name, "audex_source"),
        _ => panic!("wrong source-tool exposure"),
    }
    let payload = ToolPayload::Function {
        arguments: args.to_owned(),
    };
    let call = ToolCall {
        turn_id: "turn".into(),
        call_id: "call".into(),
        tool_name: tool.tool_name(),
        model: "fixture".into(),
        codex_turn_metadata: None,
        truncation_policy: codex_protocol::protocol::TruncationPolicy::Bytes(usize::MAX),
        source: ToolCallSource::Direct,
        conversation_history: ConversationHistory::default(),
        turn_item_emitter: Arc::new(NoopTurnItemEmitter),
        attachment_store: Arc::new(codex_attachment_store::InlineAttachmentStore),
        ephemeral: false,
        environments: Vec::new(),
        payload: payload.clone(),
    };
    tool.handle(call)
        .await
        .map(|v| v.code_mode_result(&payload))
        .map_err(|e| e.to_string())
}

#[tokio::test]
async fn actual_native_plain_and_mcp_payloads_and_errors_match() -> anyhow::Result<()> {
    for bytes in [
        b"hello\nhello\n".as_slice(),
        &[0xff, 0, 0x80, b'\n'],
        b"".as_slice(),
        "sealed 😀".as_bytes(),
    ] {
        let f = Fixture::new(bytes).await?;
        let (server_io, client_io) = tokio::io::duplex(128 * 1024);
        let service = SourceService::new(&f.snapshot).with_authority(f.authority().await?);
        let server = tokio::spawn(async move {
            SourceMcpServer::new(service)
                .serve(server_io)
                .await?
                .waiting()
                .await?;
            Ok::<(), anyhow::Error>(())
        });
        let client = ().serve(client_io).await?;
        let inventory = client.list_all_tools().await?;
        assert_eq!(
            inventory
                .iter()
                .map(|t| t.name.as_ref())
                .collect::<Vec<_>>(),
            TOOL_NAMES
        );
        for (name, args) in [
            ("list", "{}"),
            (
                "read",
                r#"{"path":"example.rs","offset":0,"max_bytes":65536}"#,
            ),
            ("read", r#"{"path":"example.rs","offset":0,"max_bytes":1}"#),
            (
                "read",
                r#"{"path":"example.rs","offset":99999,"max_bytes":1}"#,
            ),
            ("read", r#"{"path":"../other","offset":0,"max_bytes":1}"#),
            ("read", r#"{"path":"absent","offset":0,"max_bytes":1}"#),
            ("read", r#"{"path":"example.rs","offset":0,"max_bytes":0}"#),
            (
                "read",
                r#"{"path":"example.rs","offset":0,"max_bytes":65537}"#,
            ),
            ("search", r#"{"query":"hello","path":null,"max_results":2}"#),
            ("search", r#"{"query":"","path":null,"max_results":1}"#),
            ("list", r#"{"extra":true}"#),
            ("search", r#"{"query":"x","path":null,"max_results":101}"#),
        ] {
            let native = wrapper(&f.snapshot, false, name, args).await;
            let plain = wrapper(&f.snapshot, true, name, args).await;
            assert_eq!(native, plain, "{name} {args}");
            let reply = client
                .call_tool(serde_json::from_value(
                    json!({"name":name,"arguments":serde_json::from_str::<Value>(args)?}),
                )?)
                .await?;
            match native {
                Ok(value) => {
                    assert_ne!(reply.is_error, Some(true));
                    assert_eq!(reply.structured_content, Some(value.clone()));
                    let encoded = serde_json::to_value(reply.content)?;
                    assert_eq!(
                        serde_json::from_str::<Value>(encoded[0]["text"].as_str().expect("text"))?,
                        value
                    );
                }
                Err(error) => {
                    assert_eq!(reply.is_error, Some(true));
                    assert_eq!(serde_json::to_value(reply.content)?[0]["text"], error);
                    assert!(!error.contains(f.temp.path().to_str().expect("temp path")));
                }
            }
        }
        assert!(
            client
                .call_tool(serde_json::from_value(
                    json!({"name":"exec","arguments":{}})
                )?)
                .await
                .is_err()
        );
        client.cancel().await?;
        server.await??;
    }
    Ok(())
}

#[tokio::test]
async fn durable_stop_supersession_owner_and_identity_fences() -> anyhow::Result<()> {
    for action in [
        AttemptAction::Cancel,
        AttemptAction::Ignore,
        AttemptAction::Retry,
    ] {
        let f = Fixture::new(b"sealed").await?;
        let authority = f.authority().await?;
        let service = SourceService::new(&f.snapshot).with_authority(authority.clone());
        assert!(service.execute("list", "{}").await.is_ok());
        f.stop(action).await?;
        assert!(!authority.allowed().await);
        assert!(
            service
                .execute("read", r#"{"path":"example.rs","offset":0,"max_bytes":1}"#)
                .await
                .is_err()
        );
        assert!(f.authority().await.is_err());
    }
    let aborted = Fixture::new(b"sealed").await?;
    let authority = aborted.authority().await?;
    aborted
        .store
        .abort_run(&aborted.identity.run_id, &[])
        .await?;
    assert!(!authority.allowed().await);
    for field in [
        "run",
        "assignment",
        "operation",
        "execution",
        "owner",
        "generation",
        "stop",
        "snapshot",
        "manifest",
    ] {
        let f = Fixture::new(b"sealed").await?;
        let mut id = f.identity.clone();
        let mut snapshot = f.snapshot.snapshot_id().to_string();
        let mut manifest = f.manifest.clone();
        match field {
            "run" => id.run_id.push('x'),
            "assignment" => id.assignment_id.push('x'),
            "operation" => id.operation_id.push('x'),
            "execution" => id.execution_id.push('x'),
            "owner" => id.owner.push('x'),
            "generation" => id.owner_generation += 1,
            "stop" => id.run_stop_generation += 1,
            "snapshot" => snapshot.push('x'),
            _ => manifest.push('x'),
        }
        assert!(
            ReadOnlySource::open(&f.root, id, snapshot, manifest)
                .await
                .is_err(),
            "{field}"
        );
    }
    let f = Fixture::new(b"sealed").await?;
    let authority = f.authority().await?;
    sqlx::query("UPDATE operations SET lease_expires_at_ms=0 WHERE id=?")
        .bind(&f.identity.operation_id)
        .execute(&f.store.pool_for_tests())
        .await?;
    assert!(!authority.allowed().await);
    Ok(())
}

#[tokio::test]
async fn stop_overlapping_read_discards_the_result_without_blocking_control() -> anyhow::Result<()>
{
    let f = Fixture::new(b"sealed").await?;
    let mut service = SourceService::new(&f.snapshot).with_authority(f.authority().await?);
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let (e, r) = (entered.clone(), release.clone());
    service.after_access = Some(Arc::new(move || {
        let (e, r) = (e.clone(), r.clone());
        Box::pin(async move {
            e.notify_one();
            r.acquire().await.expect("barrier").forget();
        })
    }));
    let read = tokio::spawn(async move {
        service
            .execute("read", r#"{"path":"example.rs","offset":0,"max_bytes":6}"#)
            .await
    });
    entered.notified().await;
    tokio::time::timeout(Duration::from_secs(2), f.stop(AttemptAction::Cancel)).await??;
    release.add_permits(1);
    assert!(read.await?.is_err());
    Ok(())
}

#[tokio::test]
async fn source_corruption_and_unavailable_or_unsupported_authority_fail_closed()
-> anyhow::Result<()> {
    let f = Fixture::new(b"sealed").await?;
    let service = SourceService::new(&f.snapshot).with_authority(f.authority().await?);
    let source = f.snapshot.source_root().join("example.rs");
    std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o600))?;
    std::fs::write(&source, b"tamper")?;
    let error = service
        .execute("read", r#"{"path":"example.rs","offset":0,"max_bytes":6}"#)
        .await
        .expect_err("integrity");
    assert_eq!(
        error.to_string(),
        "snapshot integrity or access verification failed"
    );
    let missing = f.root.join("missing");
    assert!(
        ReadOnlySource::open(
            &missing,
            f.identity.clone(),
            f.snapshot.snapshot_id().to_string(),
            f.manifest.clone()
        )
        .await
        .is_err()
    );
    assert!(!missing.exists());
    sqlx::query("UPDATE _sqlx_migrations SET checksum=x'00' WHERE version=12")
        .execute(&f.store.pool_for_tests())
        .await?;
    assert!(f.authority().await.is_err());
    assert_eq!(
        SourceService::new(&f.snapshot)
            .execute("list", &" ".repeat(16385))
            .await
            .expect_err("arguments")
            .to_string(),
        "source tool arguments exceed 16 KiB"
    );
    Ok(())
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn stdio_binary_serves_only_bound_source_and_exits_on_eof() -> anyhow::Result<()> {
    let f = Fixture::new(b"sealed bytes").await?;
    let executable = codex_utils_cargo_bin::cargo_bin("audex")?;
    let mut child = tokio::process::Command::new(executable)
        .env_clear()
        .env("HOME", f.temp.path())
        .env("CODEX_HOME", f.root.join("bridge-home").as_path())
        .args(["audit-source-mcp", "--state-root"])
        .arg(f.root.as_path())
        .arg("--binding")
        .arg(serde_json::to_string(&f.identity)?)
        .arg("--snapshot")
        .arg(f.snapshot.snapshot_id().to_string())
        .arg("--manifest-digest")
        .arg(&f.manifest)
        .arg("--parent-pid")
        .arg(std::process::id().to_string())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let mut input = child.stdin.take().expect("stdin");
    let mut output = BufReader::new(child.stdout.take().expect("stdout")).lines();
    input.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2025-03-26\",\"capabilities\":{},\"clientInfo\":{\"name\":\"fixture\",\"version\":\"1\"}}}\n").await?;
    let init: Value = serde_json::from_str(
        &tokio::time::timeout(Duration::from_secs(20), output.next_line())
            .await??
            .expect("init"),
    )?;
    assert_eq!(init["result"]["serverInfo"]["name"], "audex-source");
    input.write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\"}\n").await?;
    let inventory: Value = serde_json::from_str(&output.next_line().await?.expect("inventory"))?;
    assert_eq!(
        inventory["result"]["tools"]
            .as_array()
            .expect("tools")
            .len(),
        3
    );
    input.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/call\",\"params\":{\"name\":\"read\",\"arguments\":{\"path\":\"example.rs\",\"offset\":0,\"max_bytes\":64}}}\n").await?;
    let read: Value = serde_json::from_str(&output.next_line().await?.expect("read"))?;
    assert_eq!(
        read["result"]["structuredContent"]["content"],
        "sealed bytes"
    );
    f.stop(AttemptAction::Cancel).await?;
    input.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":4,\"method\":\"tools/call\",\"params\":{\"name\":\"list\",\"arguments\":{}}}\n").await?;
    let denied: Value = serde_json::from_str(&output.next_line().await?.expect("denied"))?;
    assert_eq!(denied["result"]["isError"], true);
    drop(input);
    assert!(
        tokio::time::timeout(Duration::from_secs(10), child.wait())
            .await??
            .success()
    );
    assert!(output.next_line().await?.is_none());
    Ok(())
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn bridge_parent_death_is_verified_and_owned_descendants_are_reaped() -> anyhow::Result<()> {
    let f = Fixture::new(b"parent lifetime").await?;
    let script = f.temp.path().join("parent-death.py");
    std::fs::write(
        &script,
        r#"import ctypes, json, os, signal, subprocess, sys, time
assert ctypes.CDLL(None).prctl(36, 1, 0, 0, 0) == 0
inner = 'import os,subprocess,sys; p=subprocess.Popen(sys.argv[1:]+["--parent-pid",str(os.getpid())],stdin=sys.stdin,stdout=sys.stdout,stderr=subprocess.DEVNULL); print(p.pid,file=sys.stderr,flush=True); p.wait()'
parent = subprocess.Popen([sys.executable, '-c', inner] + sys.argv[1:], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
bridge = int(parent.stderr.readline())
try:
    message = {'jsonrpc':'2.0','id':1,'method':'initialize','params':{'protocolVersion':'2025-03-26','capabilities':{},'clientInfo':{'name':'parent-fixture','version':'1'}}}
    parent.stdin.write((json.dumps(message)+'\n').encode()); parent.stdin.flush()
    response = json.loads(parent.stdout.readline())
    assert response['result']['serverInfo']['name'] == 'audex-source'
    parent.kill(); parent.wait(timeout=5)
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        pid, status = os.waitpid(bridge, os.WNOHANG)
        if pid:
            assert os.WIFSIGNALED(status) and os.WTERMSIG(status) == signal.SIGKILL
            print(json.dumps({'parent_death_signal':9,'bridge_reaped':True})); break
        time.sleep(.02)
    else:
        raise RuntimeError('bridge survived parent death')
finally:
    if parent.poll() is None: parent.kill(); parent.wait()
    try: os.kill(bridge, signal.SIGKILL); os.waitpid(bridge, 0)
    except ProcessLookupError: pass
    except ChildProcessError: pass
"#,
    )?;
    let result = tokio::time::timeout(
        Duration::from_secs(25),
        tokio::process::Command::new("python3")
            .arg(&script)
            .arg(codex_utils_cargo_bin::cargo_bin("audex")?)
            .arg("audit-source-mcp")
            .arg("--state-root")
            .arg(f.root.as_path())
            .arg("--binding")
            .arg(serde_json::to_string(&f.identity)?)
            .arg("--snapshot")
            .arg(f.snapshot.snapshot_id().to_string())
            .arg("--manifest-digest")
            .arg(&f.manifest)
            .env("CODEX_HOME", f.root.join("bridge-home").as_path())
            .kill_on_drop(true)
            .output(),
    )
    .await??;
    anyhow::ensure!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let evidence: Value = serde_json::from_slice(&result.stdout)?;
    assert_eq!(evidence["bridge_reaped"], true);
    Ok(())
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "requires explicit pinned AUDEX_CLAUDE_TEST_BIN and authorised non-inference process qualification"]
async fn official_claude_inventory_and_three_tool_round_trip_against_fake_api() -> anyhow::Result<()>
{
    let f = Fixture::new(b"sealed-for-claude").await?;
    let client = std::env::var_os("AUDEX_CLAUDE_TEST_BIN").ok_or_else(|| {
        anyhow::anyhow!("select the qualified official Claude executable explicitly")
    })?;
    let script = f.temp.path().join("claude-fixture.py");
    std::fs::write(&script, include_str!("claude_fixture.py"))?;
    let result = tokio::time::timeout(
        Duration::from_secs(75),
        tokio::process::Command::new("python3")
            .arg(script)
            .arg(f.temp.path())
            .arg(client)
            .arg(codex_utils_cargo_bin::cargo_bin("audex")?)
            .arg(serde_json::to_string(&f.identity)?)
            .arg(f.snapshot.snapshot_id().to_string())
            .arg(&f.manifest)
            .kill_on_drop(true)
            .output(),
    )
    .await??;
    anyhow::ensure!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let evidence: Value = serde_json::from_slice(&result.stdout)?;
    assert_eq!(evidence["external_inference"], false);
    assert_eq!(evidence["scripted_requests"], 2);
    println!("{evidence}");
    Ok(())
}
