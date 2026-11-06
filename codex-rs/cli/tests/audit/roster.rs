//! S2 public CLI/IPC commissioning, exact controls and native attribution. Loopback only.
use super::*;
#[cfg(target_os = "linux")]
#[path = "legacy.rs"]
mod legacy;
use sqlx::Row;
use std::sync::{Arc, Mutex};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::{
    extensions::{ExtensionsConfig, compression::deflate::DeflateConfig},
    protocol::WebSocketConfig,
};
use tokio_tungstenite::{accept_hdr_async_with_config, tungstenite::Message};

type Requests = Arc<Mutex<Vec<Value>>>;

fn assignment(body: &Value) -> Value {
    body["input"]
        .as_array()
        .expect("input")
        .iter()
        .filter_map(|item| item["content"].as_array())
        .flatten()
        .filter_map(|part| part["text"].as_str())
        .filter_map(|text| serde_json::from_str::<Value>(text).ok())
        .find(|value| value.get("assignment_id").is_some())
        .expect("sealed assignment")
}

fn events(body: &Value, retained: &Value, generation: usize, mode: &str) -> Vec<Value> {
    let model = body["model"].as_str().expect("model");
    let served = match mode {
        "alias" => Some("pinned-served"),
        "mismatch" => Some("substitute"),
        "missing" | "payload_only" => None,
        "continuation_missing" | "handshake_continuation_missing" if generation == 2 => None,
        "continuation_mismatch" if generation == 2 => Some("substitute"),
        _ => Some(model),
    };
    let mut created = responses::ev_response_created(&format!("generation-{generation}"));
    if let Some(served) = served {
        created["response"]["headers"] = json!({"OpenAI-Model":served});
    }
    // response.model is deliberately insufficient; only the authoritative metadata path counts.
    created["response"]["model"] = json!(model);
    let mut events = vec![created];
    if generation == 1 {
        events.push(responses::ev_assistant_message(
            "prefix",
            "RECEIVED_UNVALIDATED_PREFIX",
        ));
        if mode == "late_mismatch" {
            events.push(
                json!({"type":"codex.response.metadata","headers":{"OpenAI-Model":"substitute"}}),
            );
        }
        events.push(responses::ev_function_call_with_namespace(
            "read",
            "audex_source",
            "read",
            r#"{"path":"example.rs","offset":0,"max_bytes":64}"#,
        ));
    } else {
        let result = json!({"schema_version":1,"snapshot_id":retained["snapshot_id"],"assignment_id":retained["assignment_id"],
            "summary":format!("Independent native review {}",retained["assignment_id"].as_str().expect("assignment")),"findings":[]});
        events.push(responses::ev_assistant_message(
            "answer",
            &result.to_string(),
        ));
    }
    events.push(responses::ev_completed(&format!("generation-{generation}")));
    events
}

struct NativeProvider {
    uri: String,
    requests: Requests,
    http: Option<MockServer>,
    task: Option<tokio::task::JoinHandle<()>>,
}
impl Drop for NativeProvider {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}
impl NativeProvider {
    async fn new(websocket: bool, mode: &'static str) -> Result<Self> {
        let requests: Requests = Arc::new(Mutex::new(Vec::new()));
        if !websocket {
            let server = responses::start_mock_server().await;
            let log = requests.clone();
            Mock::given(method("POST"))
                .and(path("/v1/responses"))
                .respond_with(move |request: &wiremock::Request| {
                    let body: Value = request.body_json().expect("native body");
                    let retained = assignment(&body);
                    let mut log = log.lock().expect("requests");
                    let generation = 1 + log
                        .iter()
                        .filter(|previous| {
                            assignment(previous)["assignment_id"] == retained["assignment_id"]
                        })
                        .count();
                    log.push(body.clone());
                    drop(log);
                    let mut response = ResponseTemplate::new(200)
                        .insert_header("content-type", "text/event-stream");
                    let mut output = events(&body, &retained, generation, mode);
                    if matches!(mode, "handshake" | "handshake_mismatch") {
                        response = response.insert_header(
                            "OpenAI-Model",
                            if mode == "handshake_mismatch" {
                                "substitute"
                            } else {
                                body["model"].as_str().expect("model")
                            },
                        );
                        output[0]["response"]
                            .as_object_mut()
                            .expect("response")
                            .remove("headers");
                    }
                    response.set_body_string(responses::sse(output))
                })
                .mount(&server)
                .await;
            return Ok(Self {
                uri: server.uri(),
                requests,
                http: Some(server),
                task: None,
            });
        }
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let uri = format!("http://{}", listener.local_addr()?);
        let log = requests.clone();
        let task = tokio::spawn(async move {
            let mut handlers = tokio::task::JoinSet::new();
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let log = log.clone();
                handlers.spawn(async move {
                    let callback=move |_:&tokio_tungstenite::tungstenite::handshake::server::Request,mut response:tokio_tungstenite::tungstenite::handshake::server::Response| {
                        if matches!(mode,"handshake"|"handshake_mismatch"|"handshake_only"|"handshake_continuation_missing") {response.headers_mut().insert("OpenAI-Model",if mode=="handshake_mismatch" {"substitute".parse().expect("header")} else {"audit-test".parse().expect("header")});}
                        Ok(response)
                    };
                    let mut extensions=ExtensionsConfig::default();extensions.permessage_deflate=Some(DeflateConfig::default());
                    let mut config=WebSocketConfig::default();config.extensions=extensions;
                    let mut socket=match accept_hdr_async_with_config(stream,callback,Some(config)).await { Ok(socket)=>socket,Err(error)=>{eprintln!("S2 fixture WebSocket handshake failed: {error}");return;} };
                    let mut retained=None;let mut generation=0;
                    while let Some(Ok(message))=socket.next().await {
                        let Message::Text(text)=message else{continue;};
                        let body:Value=serde_json::from_str(&text).expect("native WS JSON");
                        if body["type"]!="response.create" {continue;}
                        if retained.is_none() {retained=Some(assignment(&body));}
                        generation+=1;log.lock().expect("requests").push(body.clone());
                        let mut output=events(&body,retained.as_ref().expect("assignment"),generation,mode);
                        if mode=="handshake_only" || (mode=="handshake_continuation_missing" && generation==2) {output[0]["response"].as_object_mut().expect("response").remove("headers");}
                        for event in output {if socket.send(Message::Text(event.to_string().into())).await.is_err(){return;}}
                    }
                });
            }
        });
        Ok(Self {
            uri,
            requests,
            http: None,
            task: Some(task),
        })
    }
    fn count(&self) -> usize {
        self.requests.lock().expect("requests").len()
    }
    async fn shutdown(mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
            let _ = task.await;
        }
        self.http.take();
    }
}

fn configure(provider: &NativeProvider, websocket: bool) -> Result<TestDaemon> {
    let daemon = configured_url(&provider.uri)?;
    if websocket {
        let path = daemon.home.path().join("config.toml");
        let text = std::fs::read_to_string(&path)?;
        std::fs::write(path, format!("{text}\nsupports_websockets = true\n"))?;
    }
    Ok(daemon)
}
fn source(daemon: &TestDaemon) -> Result<std::path::PathBuf> {
    let path = daemon.home.path().join("example.rs");
    std::fs::write(&path, b"fn main() {}\n")?;
    Ok(path)
}
fn roster_command(
    daemon: &TestDaemon,
    path: &std::path::Path,
    key: &str,
    models: &[&str],
) -> std::process::Command {
    let mut command = audit(daemon);
    command.arg("--file").arg(path).args(["--request-key", key]);
    for model in models {
        command.args(["--reviewer", &format!("audit_fixture:{model}")]);
    }
    command
}
fn attempts(daemon: &TestDaemon, run: &str) -> Result<Vec<Value>> {
    let output = audit(daemon).args(["attempt", "list", run]).output()?;
    check(&output, 0)?;
    Ok(serde_json::from_slice::<Value>(&output.stdout)?["attempts"]
        .as_array()
        .context("attempts")?
        .clone())
}
async fn wait_attempts(
    daemon: &TestDaemon,
    run: &str,
    predicate: impl Fn(&[Value]) -> bool,
) -> Result<Vec<Value>> {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let values = attempts(daemon, run)?;
            if predicate(&values) {
                return Ok(values);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?
}
async fn pool(daemon: &TestDaemon) -> Result<sqlx::SqlitePool> {
    Ok(sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            sqlx::sqlite::SqliteConnectOptions::new()
                .filename(daemon.home.path().join("audex/audit-state.sqlite3"))
                .read_only(true),
        )
        .await?)
}
fn retain(name: &str, evidence: &Value) -> Result<()> {
    if let Some(root) = std::env::var_os("AUDEX_S2_EVIDENCE") {
        let root = std::path::PathBuf::from(root);
        std::fs::create_dir_all(&root)?;
        std::fs::write(
            root.join(format!("{name}.json")),
            serde_json::to_vec_pretty(evidence)?,
        )?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn public_three_reviewer_cli_preflight_replay_report_and_restart() -> Result<()> {
    let provider = NativeProvider::new(false, "match").await?;
    let daemon = configure(&provider, false)?;
    daemon.lifecycle("start")?;
    let path = source(&daemon)?;
    let invalid = roster_command(&daemon, &path, "bad-roster", &["audit-test", "audit-test"])
        .args(["--reviewer", "unknown:model"])
        .output()?;
    check(&invalid, 1)?;
    assert_eq!(provider.count(), 0);
    let guidance = daemon.home.path().join("explicit-guidance.md");
    std::fs::write(&guidance, "SHARED_EXPLICIT_CONTEXT")?;
    let mut command = roster_command(
        &daemon,
        &path,
        "three-native",
        &["audit-test", "audit-test", "other-native"],
    );
    command
        .args(["--brief", "SHARED_BRIEF"])
        .arg("--audex-md")
        .arg(&guidance);
    let output = command.output()?;
    check(&output, 0)?;
    let id = run_id(&output)?;
    let records = attempts(&daemon, &id)?;
    assert_eq!(records.len(), 3);
    assert_eq!(
        records
            .iter()
            .map(|v| v["model"].as_str().expect("model"))
            .collect::<Vec<_>>(),
        vec!["audit-test", "audit-test", "other-native"]
    );
    for field in ["reviewerId", "assignmentId", "operationId"] {
        assert_eq!(
            records
                .iter()
                .map(|v| v[field].as_str().expect("id"))
                .collect::<std::collections::HashSet<_>>()
                .len(),
            3
        );
    }
    let requests = provider.requests.lock().expect("requests").clone();
    assert_eq!(requests.len(), 6);
    for (ordinal, record) in records.iter().enumerate() {
        assert_eq!(record["reviewerOrdinal"], ordinal);
        assert_eq!(record["snapshotId"], records[0]["snapshotId"]);
        assert_eq!(record["completed"], true);
        let evidence = record["modelEvidence"].as_array().context("attribution")?;
        assert_eq!(evidence.len(), 2);
        assert!(
            evidence
                .iter()
                .all(|e| e["state"] == "verified" && e["observedModel"] == record["model"])
        );
        let own = requests
            .iter()
            .filter(|r| assignment(r)["assignment_id"] == record["assignmentId"])
            .collect::<Vec<_>>();
        assert_eq!(own.len(), 2);
        for request in own {
            let text = request.to_string();
            assert!(text.contains("SHARED_BRIEF"));
            assert!(text.contains("SHARED_EXPLICIT_CONTEXT"));
            assert!(!text.contains("HOST_DEVELOPER_FORBIDDEN"));
            assert!(!text.contains("AMBIENT_AGENTS_FORBIDDEN"));
            for peer in records
                .iter()
                .filter(|peer| peer["assignmentId"] != record["assignmentId"])
            {
                assert!(!text.contains(peer["assignmentId"].as_str().expect("peer")));
            }
        }
    }
    let frozen_db = pool(&daemon).await?;
    let frozen_raw: String = sqlx::query_scalar("SELECT s.input_json FROM audit_report_selections s JOIN audit_runs r ON r.authoritative_selection_id=s.id WHERE r.id=?")
        .bind(&id).fetch_one(&frozen_db).await?;
    let frozen: Value = serde_json::from_str(&frozen_raw)?;
    assert_eq!(frozen["format_version"], 5);
    for reviewer in frozen["reviewers"].as_array().context("frozen reviewers")? {
        let facts = &reviewer["provider_facts"];
        assert_eq!(facts["route"]["value"], "daemon_configured_native");
        assert_eq!(facts["executor"]["value"], "native");
        assert_eq!(facts["retained_input_version"]["value"], 2);
        let bindings = &facts["retained_bindings"]["value"];
        assert_eq!(bindings["snapshotId"], frozen["snapshot_id"]);
        assert_eq!(bindings["manifestDigest"], frozen["manifest_digest"]);
        assert_eq!(bindings["briefDigest"], frozen["brief_digest"]);
        assert_eq!(bindings["policyDigest"], reviewer["policy_digest"]);
        assert_eq!(
            facts["identity_evidence"]
                .as_array()
                .context("identity evidence")?
                .len(),
            2
        );
        let receipts = facts["receipts"].as_array().context("receipts")?;
        assert_eq!(receipts.len(), 2);
        assert!(
            receipts
                .iter()
                .all(|r| r["operation_id"] == reviewer["operation_id"]
                    && r["confirmed_at_ms"].is_number())
        );
        assert_eq!(
            facts["subscription"]["repair_occurrences"]["status"],
            "not_applicable"
        );
        assert_eq!(facts["ollama"]["resource_tag"]["status"], "not_applicable");
    }
    retain("format-four-native-bindings", &frozen)?;
    let report_text = String::from_utf8_lossy(&output.stdout);
    let mut previous = 0;
    for record in &records {
        let position = report_text
            .find(&format!(
                "## Reviewer {}:",
                record["reviewerOrdinal"].as_u64().expect("ordinal") + 1
            ))
            .context("ordered report section")?;
        assert!(position > previous);
        previous = position;
    }
    let db = pool(&daemon).await?;
    let execution: Vec<(String, String)> =
        sqlx::query_as("SELECT thread_id,turn_id FROM audit_execution ORDER BY operation_id")
            .fetch_all(&db)
            .await?;
    assert_eq!(execution.len(), 3);
    assert_eq!(
        execution
            .iter()
            .map(|e| &e.0)
            .collect::<std::collections::HashSet<_>>()
            .len(),
        3
    );
    let report = sqlx::query("SELECT id,content_digest,input_digest FROM reports WHERE run_id=?")
        .bind(&id)
        .fetch_one(&db)
        .await?;
    let report_id: String = report.try_get("id")?;
    let digest: String = report.try_get("content_digest")?;
    retain(
        "public-roster",
        &json!({"runId":id,"attempts":records,"execution":execution,"reportId":report_id,"contentDigest":digest,"inputDigest":report.get::<String,_>("input_digest"),"providerRequests":requests,"report":std::str::from_utf8(&output.stdout)?}),
    )?;
    db.close().await;
    let replay = command.arg("--detach").output()?;
    check(&replay, 0)?;
    assert_eq!(String::from_utf8_lossy(&replay.stdout).trim(), id);
    let conflict = roster_command(
        &daemon,
        &path,
        "three-native",
        &["other-native", "audit-test", "audit-test"],
    )
    .output()?;
    check(&conflict, 1)?;
    let read = audit(&daemon).args(["read", &id]).output()?;
    check(&read, 0)?;
    assert_eq!(read.stdout, output.stdout);
    daemon.lifecycle("stop")?;
    // Historical observation and acknowledged replay must not use today's selection.
    let config_path = daemon.home.path().join("config.toml");
    let changed = std::fs::read_to_string(&config_path)?
        .replace(
            "model = \"audit-test\"",
            "model = \"changed-after-completion\"",
        )
        .replace("audit_fixture", "changed-provider");
    std::fs::write(config_path, changed)?;
    daemon.lifecycle("start")?;
    let replay_after_change = command.output()?;
    check(&replay_after_change, 0)?;
    assert_eq!(
        String::from_utf8_lossy(&replay_after_change.stdout).trim(),
        id
    );
    assert_eq!(
        attempts(&daemon, &id)?,
        records,
        "historical attribution is retained, not refreshed"
    );
    let recovered = audit(&daemon).args(["read", &id]).output()?;
    check(&recovered, 0)?;
    assert_eq!(recovered.stdout, output.stdout);
    let reopened = pool(&daemon).await?;
    let retained: (String, String) =
        sqlx::query_as("SELECT id,content_digest FROM reports WHERE run_id=?")
            .bind(&id)
            .fetch_one(&reopened)
            .await?;
    assert_eq!(retained, (report_id, digest));
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM reports WHERE run_id=?")
        .bind(&id)
        .fetch_one(&reopened)
        .await?;
    assert_eq!(count, 1, "restart does not publish another report");
    reopened.close().await;
    assert_eq!(provider.count(), 6);
    daemon.lifecycle("stop")?;
    provider.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_http_and_websocket_identity_is_required_for_every_generation() -> Result<()> {
    for websocket in [false, true] {
        for mode in [
            "match",
            "handshake",
            "handshake_mismatch",
            "alias",
            "missing",
            "payload_only",
            "mismatch",
            "late_mismatch",
            "continuation_missing",
            "continuation_mismatch",
            "handshake_only",
            "handshake_continuation_missing",
        ] {
            if !websocket && mode.starts_with("handshake_") && mode != "handshake_mismatch" {
                continue;
            }
            let provider = NativeProvider::new(websocket, mode).await?;
            let daemon = configure(&provider, websocket)?;
            daemon.lifecycle("start")?;
            let path = source(&daemon)?;
            let mut command = audit(&daemon);
            command.arg("--file").arg(&path).args([
                "--model",
                "audit-test",
                "--provider",
                "audit_fixture",
                "--detach",
            ]);
            if mode == "alias" {
                command.args(["--expected-served-model", "pinned-served"]);
            }
            let output = command.output()?;
            check(&output, 0)?;
            let id = String::from_utf8(output.stdout)?.trim().to_owned();
            let records = wait_attempts(&daemon, &id, |r| {
                r[0]["completed"] == true || r[0]["operationState"] == "failed"
            })
            .await?;
            let valid = matches!(mode, "match" | "handshake" | "alias");
            assert_eq!(
                records[0]["completed"], valid,
                "{websocket}/{mode}: {records:?}"
            );
            let evidence = records[0]["modelEvidence"]
                .as_array()
                .context("model evidence")?;
            let expected_count = if valid
                || mode.starts_with("continuation_")
                || mode == "handshake_continuation_missing"
            {
                2
            } else {
                1
            };
            let expected_requests = if websocket && mode == "handshake_mismatch" {
                0
            } else {
                expected_count
            };
            assert_eq!(
                provider.count(),
                expected_requests,
                "no post-disqualification submission: {websocket}/{mode}"
            );
            assert_eq!(evidence.len(), expected_count);
            if matches!(mode, "handshake_only" | "handshake_continuation_missing") {
                let last = evidence.last().context("connection evidence")?;
                assert_eq!(last["state"], "missing");
                assert_eq!(
                    last["evidenceScope"], "connection",
                    "connection identity is never promoted to a generation attestation"
                );
            }
            if valid {
                assert!(
                    evidence
                        .iter()
                        .all(|e| e["state"] == "verified" && e["evidenceScope"] == "generation")
                );
            } else {
                assert!(
                    evidence
                        .iter()
                        .any(|e| e["state"] == "missing" || e["state"] == "mismatch")
                );
            }
            assert!(evidence.iter().all(|e| e["transport"]
                == if websocket {
                    "native_websocket"
                } else {
                    "native_http"
                }));
            let assignment = records[0]["assignmentId"].as_str().context("assignment")?;
            let observed = audit(&daemon)
                .args(["attempt", "observe", &id, assignment])
                .output()?;
            check(&observed, 0)?;
            let waited = audit(&daemon)
                .args(["attempt", "wait", &id, assignment])
                .output()?;
            check(&waited, if valid { 0 } else { 5 })?;
            assert_eq!(
                provider.count(),
                expected_requests,
                "read-only commands do not infer"
            );
            if !valid {
                let pending = audit(&daemon).args(["read", &id]).output()?;
                check(&pending, 5)?;
                let ignored = audit(&daemon)
                    .args(["attempt", "ignore", &id, assignment])
                    .output()?;
                check(&ignored, 0)?;
            }
            let published = audit(&daemon).args(["read", &id, "--follow"]).output()?;
            check(&published, if valid { 0 } else { 3 })?;
            let report = String::from_utf8_lossy(&published.stdout);
            if valid {
                assert!(report.contains("Independent native review"));
            } else {
                assert!(!report.contains("- Completeness: completed"));
                assert!(report.contains("Attribution failed"));
            }
            if matches!(
                mode,
                "late_mismatch" | "continuation_missing" | "continuation_mismatch"
            ) {
                assert!(
                    String::from_utf8_lossy(&observed.stdout)
                        .contains("RECEIVED_UNVALIDATED_PREFIX")
                );
            }
            retain(
                &format!("identity-{}-{mode}", if websocket { "ws" } else { "http" }),
                &json!({"runId":id,"attempts":records,"requestCount":provider.count(),"report":report}),
            )?;
            assert_eq!(provider.count(), expected_requests);
            daemon.lifecycle("stop")?;
            provider.shutdown().await;
        }
    }
    Ok(())
}

fn control(
    daemon: &TestDaemon,
    run: &str,
    assignment: &str,
    action: &str,
    key: &str,
) -> Result<Output> {
    let mut command = audit(daemon);
    command.args(["attempt", action, run, assignment, "--action-key", key]);
    if action == "retry" {
        command.args(["--authorise-retry", "--acknowledge-possible-overlap"]);
    }
    Ok(command.output()?)
}
fn replacement(output: &Output) -> Result<String> {
    check(output, 0)?;
    Ok(
        serde_json::from_slice::<Value>(&output.stdout)?["replacementAssignmentId"]
            .as_str()
            .context("replacement")?
            .to_owned(),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn public_queued_rosters_and_exact_attempt_controls_survive_restart() -> Result<()> {
    use core_test_support::streaming_sse::{StreamingSseChunk, start_streaming_sse_server};
    for sequential in [false, true] {
        let mut releases = Vec::new();
        let mut scripts = Vec::new();
        for index in 0..10 {
            let (release, gate) = tokio::sync::oneshot::channel();
            releases.push(release);
            let mut created = responses::ev_response_created(&format!("held-{index}"));
            created["response"]["headers"] = json!({"OpenAI-Model":"audit-test"});
            scripts.push(vec![
                StreamingSseChunk {
                    gate: None,
                    body: responses::sse(vec![
                        created,
                        responses::ev_message_item_added("partial", ""),
                        responses::ev_output_text_delta("CONTROLLED_PARTIAL"),
                    ]),
                },
                StreamingSseChunk {
                    gate: Some(gate),
                    body: responses::sse(vec![responses::ev_completed(&format!("held-{index}"))]),
                },
            ]);
        }
        eprintln!("S2 scheduler case sequential={sequential}");
        let (provider, _) = start_streaming_sse_server(scripts).await;
        let daemon = configured_url(provider.uri())?;
        daemon.lifecycle("start")?;
        let path = source(&daemon)?;
        let mut command = roster_command(&daemon, &path, "queued-native", &["audit-test"; 4]);
        command.arg("--detach");
        if sequential {
            command.arg("--sequential");
        }
        let output = command.output()?;
        check(&output, 0)?;
        let id = String::from_utf8(output.stdout)?.trim().to_owned();
        let capacity = if sequential { 1 } else { 3 };
        tokio::time::timeout(
            Duration::from_secs(15),
            provider.wait_for_request_count(capacity),
        )
        .await?;
        let initial = wait_attempts(&daemon, &id, |r| {
            r.iter()
                .filter(|v| v["operationState"] == "accepted")
                .count()
                == capacity
        })
        .await?;
        assert_eq!(initial.len(), 4);
        assert_eq!(
            provider.requests().await.len(),
            capacity,
            "remaining reviewers stay queued"
        );
        let ids = initial
            .iter()
            .map(|v| v["assignmentId"].as_str().expect("assignment").to_owned())
            .collect::<Vec<_>>();
        for action in ["list", "read", "observe"] {
            let mut read = audit(&daemon);
            read.args(["attempt", action, &id]);
            if action != "list" {
                read.arg(&ids[0]);
            }
            check(&read.output()?, 0)?;
        }
        assert_eq!(
            provider.requests().await.len(),
            capacity,
            "read/observe does not infer"
        );
        if !sequential {
            let waiter = audit(&daemon)
                .args(["attempt", "wait", &id, &ids[0]])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?;
            let retried = control(&daemon, &id, &ids[0], "retry", "direct-active-retry")?;
            let new0 = replacement(&retried)?;
            let waited = waiter.wait_with_output()?;
            check(&waited, 3)?;
            assert_eq!(
                serde_json::from_slice::<Value>(&waited.stdout)?["attempt"]["assignmentId"],
                ids[0]
            );
            check(
                &control(&daemon, &id, &ids[0], "ignore", "stale-ignore")?,
                1,
            )?;
            let replay = control(&daemon, &id, &ids[0], "retry", "direct-active-retry")?;
            assert_eq!(replacement(&replay)?, new0);
            check(&control(&daemon, &id, &ids[1], "cancel", "cancel-peer")?, 0)?;
            tokio::time::timeout(Duration::from_secs(15), provider.wait_for_request_count(5))
                .await
                .context("fourth reviewer and direct replacement dispatch")?;
            let new1 = replacement(&control(
                &daemon,
                &id,
                &ids[1],
                "retry",
                "retry-cancelled-peer",
            )?)?;
            check(&control(&daemon, &id, &ids[2], "ignore", "ignore-peer")?, 0)?;
            check(
                &control(&daemon, &id, &ids[2], "retry", "retry-ignored-peer")?,
                1,
            )?;
            tokio::time::timeout(Duration::from_secs(15), provider.wait_for_request_count(6))
                .await
                .context("cancelled peer Retry dispatch")?;
            check(
                &control(&daemon, &id, &new0, "cancel", "cancel-replacement")?,
                0,
            )?;
            let new00 = replacement(&control(
                &daemon,
                &id,
                &new0,
                "retry",
                "retry-cancelled-replacement",
            )?)?;
            tokio::time::timeout(Duration::from_secs(15), provider.wait_for_request_count(7))
                .await
                .context("replacement Retry dispatch")?;
            check(
                &control(&daemon, &id, &new00, "ignore", "permanent-ignore")?,
                0,
            )?;
            check(
                &control(&daemon, &id, &new00, "retry", "retry-permanent-ignore")?,
                1,
            )?;
            check(&control(&daemon, &id, &new1, "cancel", "settle-peer")?, 0)?;
            check(&control(&daemon, &id, &ids[3], "cancel", "settle-last")?, 0)?;
            check(
                &control(&daemon, &id, &ids[3], "retry", "retry-after-readiness")?,
                1,
            )?;
            assert_eq!(provider.requests().await.len(), 7);
        } else {
            // Observe a submitted first reviewer and three durable queued assignments,
            // then kill only the daemon belonging to this fixture home.
            let pid = daemon.pid("app-server.pid")?;
            let environment = std::fs::read(format!("/proc/{pid}/environ"))?;
            let expected = format!("CODEX_HOME={}", daemon.home.path().display());
            ensure!(
                environment
                    .split(|b| *b == 0)
                    .any(|v| v == expected.as_bytes()),
                "refuse unowned PID"
            );
            let cmdline = std::fs::read(format!("/proc/{pid}/cmdline"))?;
            ensure!(
                cmdline
                    .windows(b"app-server".len())
                    .any(|v| v == b"app-server"),
                "refuse non-daemon PID"
            );
            signal(pid, libc::SIGKILL)?;
            wait_for_exit(pid)?;
            daemon.lifecycle("start")?;
            let replay = command.output()?;
            check(&replay, 0)?;
            assert_eq!(String::from_utf8_lossy(&replay.stdout).trim(), id);
            let db = pool(&daemon).await?;
            tokio::time::timeout(Duration::from_secs(45), async {
                loop {
                    let pending:bool=sqlx::query_scalar("SELECT COALESCE(retry_classification='observe_only',0) FROM operations WHERE assignment_id=?")
                        .bind(&ids[0]).fetch_one(&db).await?;
                    if pending {return anyhow::Ok(());}
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }).await.context("observe interrupted execution after its retained lease")??;
            let pending = audit(&daemon).args(["read", &id]).output()?;
            check(&pending, 5)?;
            assert_eq!(
                provider.requests().await.len(),
                1,
                "uncertain execution is neither replayed nor silently released"
            );
            let recovered = attempts(&daemon, &id)?;
            assert_eq!(
                recovered
                    .iter()
                    .map(|v| v["assignmentId"].as_str().expect("assignment"))
                    .collect::<Vec<_>>(),
                ids.iter().map(String::as_str).collect::<Vec<_>>()
            );
            let generation_counts:Vec<(String,i64)>=sqlx::query_as("SELECT assignment_id,count(*) FROM audit_request_admissions GROUP BY assignment_id ORDER BY assignment_id").fetch_all(&db).await?;
            assert!(
                generation_counts.iter().all(|(_, count)| *count == 1),
                "no inference replay on restart"
            );
            db.close().await;
            retain(
                "queued-restart",
                &json!({"runId":id,"killedPid":pid,"boundary":"first reviewer submitted; three assignments planned","initial":initial,"recovered":recovered,"generationCounts":generation_counts}),
            )?;
            check(
                &control(
                    &daemon,
                    &id,
                    &ids[0],
                    "cancel",
                    "release-uncertain-exact-attempt",
                )?,
                0,
            )?;
            tokio::time::timeout(Duration::from_secs(15), provider.wait_for_request_count(2))
                .await?;
            for (index, assignment) in ids.iter().enumerate().skip(1) {
                check(
                    &control(
                        &daemon,
                        &id,
                        assignment,
                        "ignore",
                        &format!("sequential-settle-{index}"),
                    )?,
                    0,
                )?;
                if index < 3 {
                    tokio::time::timeout(
                        Duration::from_secs(15),
                        provider.wait_for_request_count(index + 2),
                    )
                    .await?;
                }
            }
            assert_eq!(provider.requests().await.len(), 4);
        }
        let report = audit(&daemon).args(["read", &id, "--follow"]).output()?;
        check(&report, 3)?;
        let report_text = String::from_utf8_lossy(&report.stdout);
        assert!(report_text.contains("CONTROLLED_PARTIAL"));
        let final_attempts = attempts(&daemon, &id)?;
        assert!(
            final_attempts
                .iter()
                .filter(|a| a["current"] == true)
                .all(|a| a["retryEligible"] == false)
        );
        retain(
            if sequential {
                "sequential-controls"
            } else {
                "concurrent-controls"
            },
            &json!({"runId":id,"attempts":final_attempts,"providerRequests":provider.requests().await.len(),"report":report_text}),
        )?;
        daemon.lifecycle("stop")?;
        drop(releases);
        provider.shutdown().await;
    }
    Ok(())
}
