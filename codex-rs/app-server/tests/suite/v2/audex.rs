//! Process-level coverage of production service ownership and its ordinary event reader.
use app_test_support::TestAppServer;
use codex_app_server_protocol::{
    AuditReadResponse, AuditRunPhase, AuditStartResponse, ClientRequest, ThreadStartParams,
};
use core_test_support::responses;
use serde_json::{Value, json};
use std::time::Duration;
use wiremock::{
    Mock, ResponseTemplate,
    matchers::{method, path},
};

#[tokio::test]
async fn audit_api_runs_real_codex_tools_and_leaves_ordinary_threads_unchanged()
-> anyhow::Result<()> {
    let server = responses::start_mock_server().await;
    let home = tempfile::tempdir()?;
    std::fs::write(
        home.path().join("config.toml"),
        format!(
            r#"
model = "audit-test"
model_provider = "local_audit_test"
developer_instructions = "HOST_INSTRUCTION_SENTINEL"
[model_providers.local_audit_test]
name = "Local deterministic model"
base_url = "{}/v1"
wire_api = "responses"
request_max_retries = 0
stream_max_retries = 0
"#,
            server.uri()
        ),
    )?;
    std::fs::write(home.path().join("AGENTS.md"), "GLOBAL_AGENTS_SENTINEL")?;
    Mock::given(method("POST")).and(path("/v1/responses")).respond_with(|request: &wiremock::Request| {
        let body: Value = serde_json::from_slice(&request.body).expect("request JSON");
        let text = body.to_string();
        assert!(!text.contains("HOST_INSTRUCTION_SENTINEL"));
        assert!(!text.contains("GLOBAL_AGENTS_SENTINEL"));
        assert_eq!(body["tools"].as_array().expect("only audit tools").len(), 1);
        assert_eq!(body["tools"][0]["name"], "audex_source");
        let input = body["input"].as_array().expect("input");
        let output = if input.iter().any(|item| item["type"] == "function_call_output") {
            assert!(text.contains("fn main()"));
            let assignment = input.iter().filter_map(|item| item["content"].as_array()).flatten()
                .filter_map(|part| part["text"].as_str()).filter_map(|text| serde_json::from_str::<Value>(text).ok())
                .find(|value| value.get("assignment_id").is_some()).expect("audit assignment");
            let result = json!({"schema_version":1,"snapshot_id":assignment["snapshot_id"],"assignment_id":assignment["assignment_id"],"summary":"Process-level local audit","findings":[]});
            responses::sse(vec![responses::ev_assistant_message("answer", &result.to_string()), responses::ev_completed("final")])
        } else {
            assert!(!text.contains("fn main()"));
            responses::sse(vec![responses::ev_function_call_with_namespace("read", "audex_source", "read", r#"{"path":"example.rs","offset":0,"max_bytes":64}"#),responses::ev_completed("read")])
        };
        ResponseTemplate::new(200).insert_header("content-type", "text/event-stream").set_body_string(output)
    }).expect(2).mount(&server).await;
    let mut client = TestAppServer::builder()
        .with_codex_home(home.path())
        .build_initialized()
        .await?;
    let started: AuditStartResponse = client.request(|id| ClientRequest::AuditStart { request_id:id, params:serde_json::from_value(json!({
        "requestKey":"process-audit", "source":{"type":"snippet","name":"example.rs","content":"fn main() {}\n"},
        "brief":"Static audit", "provider":"local_audit_test", "model":"audit-test"
    })).expect("typed audit params") }).await?;
    let run_id = started.run.run_id;
    let completed = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let read: AuditReadResponse = client
                .request(|id| ClientRequest::AuditRead {
                    request_id: id,
                    params: serde_json::from_value(json!({"runId":run_id})).expect("read params"),
                })
                .await?;
            if matches!(
                read.run.phase,
                AuditRunPhase::Completed
                    | AuditRunPhase::Failed
                    | AuditRunPhase::CompletedWithFailures
            ) {
                return anyhow::Ok(read.run);
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await??;
    assert_eq!(completed.phase, AuditRunPhase::Completed, "{completed:?}");
    assert!(
        completed
            .report
            .expect("report")
            .contains("Process-level local audit")
    );
    let ordinary = client.start_thread(ThreadStartParams::default()).await?;
    assert_eq!(ordinary.model, "audit-test");
    assert!(
        serde_json::to_value(&ordinary.instruction_sources)?
            .to_string()
            .contains("AGENTS.md"),
        "ordinary project instruction discovery must remain active"
    );
    assert_ne!(
        ordinary.thread.thread_source,
        Some(codex_app_server_protocol::ThreadSource::Feature(
            "audex-reviewer".to_string()
        ))
    );
    assert_eq!(server.received_requests().await.expect("requests").len(), 2);
    let sqlite = codex_state::SqliteConfig::new_for_testing(
        codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(home.path())?,
    );
    let pool = sqlite
        .open_read_only_pool(
            &home.path().join("audex/audit-state.sqlite3"),
            Some(Duration::from_secs(5)),
        )
        .await?;
    let reviewer: String = sqlx::query_scalar("SELECT thread_id FROM audit_execution")
        .fetch_one(&pool)
        .await?;
    pool.close().await;
    let private_path = codex_rollout::find_thread_path_by_id_str(home.path(), &reviewer, None)
        .await?
        .expect("private rollout path");
    for (method, params) in [
        (
            "thread/read",
            json!({"threadId":reviewer,"includeTurns":true}),
        ),
        (
            "thread/resume",
            json!({"threadId":reviewer,"developerInstructions":"Change private reviewer policy"}),
        ),
        ("thread/fork", json!({"threadId":reviewer})),
        (
            "thread/fork",
            json!({"threadId":ordinary.thread.id,"path":private_path}),
        ),
        (
            "thread/resume",
            json!({"threadId":ordinary.thread.id,"path":private_path}),
        ),
        ("thread/archive", json!({"threadId":reviewer})),
        ("thread/turns/list", json!({"threadId":reviewer})),
        ("thread/attachment/list", json!({"threadId":reviewer})),
        (
            "thread/searchOccurrences",
            json!({"threadId":reviewer,"searchTerm":"audit"}),
        ),
        (
            "turn/start",
            json!({"threadId":reviewer,"input":[{"type":"text","text":"Unsolicited extra inference"}]}),
        ),
        (
            "turn/interrupt",
            json!({"threadId":reviewer,"turnId":"private-turn"}),
        ),
    ] {
        let _: ClientRequest =
            serde_json::from_value(json!({"id":0,"method":method,"params":params}))
                .expect("typed private-thread request");
        let id = client.send_request(method, Some(params)).await?;
        let denied = tokio::time::timeout(
            Duration::from_secs(10),
            client
                .read_stream_until_error_message(codex_app_server_protocol::RequestId::Integer(id)),
        )
        .await??;
        assert!(
            denied
                .error
                .message
                .contains("Audex reviewer threads are private"),
            "{method}: {denied:?}"
        );
    }
    let loaded: codex_app_server_protocol::ThreadLoadedListResponse = client
        .request(|id| ClientRequest::ThreadLoadedList {
            request_id: id,
            params: Default::default(),
        })
        .await?;
    assert!(!loaded.data.contains(&reviewer));
    assert!(loaded.data.contains(&ordinary.thread.id));
    assert_eq!(
        server.received_requests().await.expect("requests").len(),
        2,
        "denied private-thread APIs must not submit inference"
    );
    client.shutdown_gracefully().await?;
    let mut cold = TestAppServer::builder()
        .with_codex_home(home.path())
        .build_initialized()
        .await?;
    let params = codex_app_server_protocol::ThreadResumeParams {
        thread_id: reviewer,
        ..Default::default()
    };
    let id = cold
        .send_request("thread/resume", Some(serde_json::to_value(params)?))
        .await?;
    let denied = cold
        .read_stream_until_error_message(codex_app_server_protocol::RequestId::Integer(id))
        .await?;
    assert!(
        denied
            .error
            .message
            .contains("Audex reviewer threads are private")
    );
    let retained: AuditReadResponse = cold
        .request(|id| ClientRequest::AuditRead {
            request_id: id,
            params: codex_app_server_protocol::AuditReadParams { run_id },
        })
        .await?;
    assert_eq!(
        retained.run.phase,
        AuditRunPhase::Completed,
        "audit-owned retrieval remains available after restart"
    );
    Ok(())
}
