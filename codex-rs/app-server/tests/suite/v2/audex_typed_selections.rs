//! Public wire rejection through the real service, with all provider HTTP recorded.
use super::*;
use codex_app_server_protocol::{
    AuditCapabilitiesParams, AuditCapabilitiesResponse, AuditListParams, AuditListResponse,
    RequestId,
};

#[tokio::test]
async fn typed_selections_reject_atomically_without_provider_or_catalogue_activity()
-> anyhow::Result<()> {
    let server = responses::start_mock_server().await;
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
name = "Recorded local fixture"
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
    let capability: AuditCapabilitiesResponse = client
        .request(|id| ClientRequest::AuditCapabilities {
            request_id: id,
            params: AuditCapabilitiesParams {},
        })
        .await?;
    assert_eq!(capability.protocol_version, 8);
    let base = json!({"requestKey":"rejected", "source":{"type":"snippet", "name":"x.rs", "content":"source"},
        "brief":"brief", "reviewers":[{"provider":"local_audit_test", "model":"audit-test"},
        {"provider":"local_audit_test", "model":"audit-test"}, {"provider":"local_audit_test", "model":"audit-test"}]});
    let mut rejected = Vec::new();
    for (executor, route) in [
        ("claude_client", "claude_subscription"),
        ("claude_client", "claude_api"),
        ("native", "ollama_local"),
        ("native", "claude_subscription"),
        ("native", "claude_api"),
        ("claude_client", "daemon_configured_native"),
        ("claude_client", "ollama_local"),
        ("unknown", "daemon_configured_native"),
        ("native", "unknown"),
    ] {
        for position in 0..3 {
            let mut params = base.clone();
            params["reviewers"][position]["executionAdapter"] = json!(executor);
            params["reviewers"][position]["authenticationRoute"] = json!(route);
            rejected.push(params);
        }
    }
    for roster in [
        json!([]),
        json!([{}]),
        json!([{"provider":"local_audit_test"}]),
        json!([{"provider":"", "model":"audit-test"}]),
        json!([{"provider":"local_audit_test", "model":" "}]),
        json!([{"provider":"local_audit_test", "model":"audit-test", "authenticationRoute":null}]),
        json!([{"provider":"local_audit_test", "model":"audit-test", "unexpected":true}]),
    ] {
        let mut params = base.clone();
        params["reviewers"] = roster;
        rejected.push(params);
    }
    for field in [
        "provider",
        "model",
        "reasoningEffort",
        "expectedServedModel",
    ] {
        let mut params = base.clone();
        params[field] = json!("conflict");
        rejected.push(params);
    }
    for params in rejected {
        for (method, payload) in [
            ("audit/preflight", json!({"request":params})),
            ("audit/start", params.clone()),
        ] {
            let id = client.send_request(method, Some(payload)).await?;
            let error = tokio::time::timeout(
                Duration::from_secs(10),
                client.read_stream_until_error_message(RequestId::Integer(id)),
            )
            .await??;
            assert!(!error.error.message.is_empty(), "{method}: {params}");
        }
        let list: AuditListResponse = client
            .request(|id| ClientRequest::AuditList {
                request_id: id,
                params: AuditListParams {
                    after: None,
                    limit: Some(100),
                },
            })
            .await?;
        assert!(list.runs.is_empty(), "no partial commission: {params}");
        assert!(
            server.received_requests().await.unwrap().is_empty(),
            "no inference, model activation or catalogue HTTP: {params}"
        );
    }
    let sqlite = codex_state::SqliteConfig::new_for_testing(
        codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(home.path())?,
    );
    let pool = sqlite
        .open_read_only_pool(
            &home.path().join("audex/audit-state.sqlite3"),
            Some(Duration::from_secs(5)),
        )
        .await?;
    for query in [
        "SELECT COUNT(*) FROM audit_runs",
        "SELECT COUNT(*) FROM audit_reviewers",
        "SELECT COUNT(*) FROM audit_attempts",
        "SELECT COUNT(*) FROM operations",
        "SELECT COUNT(*) FROM audit_execution",
        "SELECT COUNT(*) FROM audit_request_admissions",
    ] {
        let count: i64 = sqlx::query_scalar(query).fetch_one(&pool).await?;
        assert_eq!(count, 0, "no runnable member or execution: {query}");
    }
    pool.close().await;
    assert!(
        !home.path().join("audex/snapshots").exists(),
        "invalid selections rejected before source capture"
    );
    Ok(())
}
