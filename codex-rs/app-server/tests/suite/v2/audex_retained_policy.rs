//! Actual service start/Retry/restart over retained source and policy; loopback only.
use super::*;
use codex_app_server_protocol::{AuditAttemptControlResponse, AuditRunView, RequestId};

async fn failed_attempt(client: &mut TestAppServer, run: &str) -> anyhow::Result<AuditRunView> {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let read: AuditReadResponse = client
                .request(|id| ClientRequest::AuditRead {
                    request_id: id,
                    params: serde_json::from_value(json!({"runId":run})).unwrap(),
                })
                .await?;
            if read.run.reviewers[0].assignment_state == "failed" {
                return anyhow::Ok(read.run);
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await?
}

#[tokio::test]
async fn retained_policy_drift_refusal_retry_and_cold_replay_use_real_service() -> anyhow::Result<()>
{
    let server = responses::start_mock_server().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(500))
        .expect(2)
        .mount(&server)
        .await;
    let home = tempfile::tempdir()?;
    std::fs::write(
        home.path().join("config.toml"),
        format!(
            r#"
model = "audit-test"
model_provider = "local_audit_test"
[features]
plugins = false
remote_plugin = false
plugin_discovery_prefetch = false
[model_providers.local_audit_test]
name = "Retained contract fixture"
base_url = "{}/v1"
wire_api = "responses"
request_max_retries = 0
stream_max_retries = 0
"#,
            server.uri()
        ),
    )?;
    let mut client = TestAppServer::builder()
        .with_codex_home(home.path())
        .build_initialized()
        .await?;
    let params = json!({"requestKey":"retained-process","source":{"type":"snippet","name":"example.rs","content":"fn main() {}\n"},
        "brief":"Static audit","provider":"local_audit_test","model":"audit-test"});
    let started: AuditStartResponse = client
        .request(|id| ClientRequest::AuditStart {
            request_id: id,
            params: serde_json::from_value(params.clone()).unwrap(),
        })
        .await?;
    let run = failed_attempt(&mut client, &started.run.run_id).await?;
    let original = run.reviewers[0].clone();
    let sqlite = codex_state::SqliteConfig::new_for_testing(
        codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(home.path())?,
    );
    let pool = sqlite
        .open_read_only_pool(
            &home.path().join("audex/audit-state.sqlite3"),
            Some(Duration::from_secs(5)),
        )
        .await?;
    let retained: (String,String,String)=sqlx::query_as("SELECT i.request_json,i.policy_json,e.thread_id FROM audit_attempt_inputs i JOIN operations o ON o.assignment_id=i.assignment_id JOIN audit_execution e ON e.operation_id=o.id WHERE i.assignment_id=?")
        .bind(&original.assignment_id).fetch_one(&pool).await?;
    assert_eq!(serde_json::from_str::<Value>(&retained.0)?["version"], 2);
    let settings = home.path().join("audex/reviewer-backends.json");
    std::fs::write(&settings, b"{\"version\":999}")?;
    let command = json!({"runId":run.run_id,"assignmentId":original.assignment_id,"actionKey":"explicit-retry","action":"retry","reason":"fixture retry","authoriseRetry":true,"acknowledgePossibleOverlap":true});
    let id = client
        .send_request("audit/attempt/control", Some(command.clone()))
        .await?;
    let error = client
        .read_stream_until_error_message(RequestId::Integer(id))
        .await?;
    assert!(error.error.message.contains("unsupported"), "{error:?}");
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM audit_attempts")
            .fetch_one(&pool)
            .await?,
        1
    );
    // Unrelated route edits must not prevent the one explicitly authorised Retry.
    std::fs::write(
        &settings,
        serde_json::to_vec(&json!({"version":1,
        "claudeSubscription":{"executable":"/absent/claude","clientVersion":"2.1.314","profile":"/absent/profile","runtimeRoot":"/absent/private"},
        "claudeApi":{"executable":"/absent/claude","clientVersion":"2.1.314","runtimeRoot":"/absent/api","credential":{"kind":"environment","reference":"UNRESOLVED_FIXTURE_KEY"},"upstream":"https://api.example.test"},
        "ollamaLocal":{"endpoint":"http://127.0.0.1:11434","resourceTag":"fixture","model":"fixture","modelDigest":"digest","contextTokens":8192,"outputTokens":1024}}))?,
    )?;
    let retry: AuditAttemptControlResponse = client
        .request(|id| ClientRequest::AuditAttemptControl {
            request_id: id,
            params: serde_json::from_value(command.clone()).unwrap(),
        })
        .await?;
    let retried = failed_attempt(&mut client, &run.run_id).await?;
    let replacement = &retried.reviewers[0];
    assert_ne!(replacement.assignment_id, original.assignment_id);
    assert_ne!(replacement.operation_id, original.operation_id);
    assert_eq!(replacement.snapshot_id, original.snapshot_id);
    let fresh: (String,String,String)=sqlx::query_as("SELECT i.request_json,i.policy_json,e.thread_id FROM audit_attempt_inputs i JOIN operations o ON o.assignment_id=i.assignment_id JOIN audit_execution e ON e.operation_id=o.id WHERE i.assignment_id=?")
        .bind(&replacement.assignment_id).fetch_one(&pool).await?;
    assert_eq!(fresh.0, retained.0);
    assert_eq!(fresh.1, retained.1);
    assert_ne!(fresh.2, retained.2);
    let mut changed = params.clone();
    changed["expectedServedModel"] = json!("other");
    let id = client.send_request("audit/start", Some(changed)).await?;
    let error = client
        .read_stream_until_error_message(RequestId::Integer(id))
        .await?;
    assert_eq!(
        error.error.data.unwrap()["reason"],
        "auditIdempotencyConflict"
    );
    client.shutdown_gracefully().await?;
    std::fs::write(&settings, b"{\"version\":999}")?;
    let mut cold = TestAppServer::builder()
        .with_codex_home(home.path())
        .build_initialized()
        .await?;
    let replay: AuditAttemptControlResponse = cold
        .request(|id| ClientRequest::AuditAttemptControl {
            request_id: id,
            params: serde_json::from_value(command).unwrap(),
        })
        .await?;
    assert_eq!(serde_json::to_value(replay)?, serde_json::to_value(retry)?);
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM audit_execution")
            .fetch_one(&pool)
            .await?,
        2
    );
    pool.close().await;
    cold.shutdown_gracefully().await?;
    Ok(())
}
