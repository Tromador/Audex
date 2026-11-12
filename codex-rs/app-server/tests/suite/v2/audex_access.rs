//! Exercise the ownership guard against real stored identity and filesystem failures.
use app_test_support::{MockResponsesConfig, TestAppServer, create_fake_rollout, rollout_path};
use codex_app_server_protocol::{ClientRequest, RequestId, ThreadAttachmentListResponse};
use codex_protocol::ThreadId;
use codex_utils_absolute_path::AbsolutePathBuf;
use serde_json::json;
use std::time::Duration;

#[cfg(unix)]
#[tokio::test]
async fn attachment_probe_allows_absence_but_rejects_invalid_identity_and_io() -> anyhow::Result<()>
{
    use std::os::unix::fs::PermissionsExt;

    let server = core_test_support::responses::start_mock_server().await;
    let home = tempfile::tempdir()?;
    MockResponsesConfig::new(&server.uri()).write(home.path())?;
    let runtime = codex_state::StateRuntime::init(
        codex_state::SqliteConfig::new_for_testing(AbsolutePathBuf::from_absolute_path(
            home.path(),
        )?),
        "mock_provider".to_owned(),
    )
    .await?;
    runtime.mark_backfill_complete(None).await?;
    let mut client = TestAppServer::builder()
        .with_codex_home(home.path())
        .without_auto_env()
        .build_initialized()
        .await?;
    let absent = ThreadId::new();
    let empty: ThreadAttachmentListResponse = client
        .request(|id| ClientRequest::ThreadAttachmentList {
            request_id: id,
            params: serde_json::from_value(json!({"threadId":absent,"limit":1})).unwrap(),
        })
        .await?;
    assert!(empty.data.is_empty());
    assert!(empty.next_cursor.is_none());

    for case in ["corrupt", "mismatch", "io"] {
        let stored_id = create_fake_rollout(
            home.path(),
            "2025-02-01T10-00-00",
            "2025-02-01T10:00:00Z",
            "guard fixture",
            Some("mock_provider"),
            None,
        )?;
        let path = rollout_path(home.path(), "2025-02-01T10-00-00", &stored_id);
        let requested_id = if case == "mismatch" {
            ThreadId::new()
        } else {
            ThreadId::from_string(&stored_id)?
        };
        let builder = codex_state::ThreadMetadataBuilder::new(
            requested_id,
            path.clone(),
            chrono::Utc::now(),
            codex_protocol::protocol::SessionSource::Cli,
        )
        .with_cwd(home.path().to_path_buf());
        runtime
            .upsert_thread(&builder.build("mock_provider"))
            .await?;
        let permissions = std::fs::metadata(&path)?.permissions();
        match case {
            "corrupt" => std::fs::write(&path, "invalid session header\n")?,
            "io" => std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0))?,
            _ => {}
        }
        let result = async {
            let id = client
                .send_request(
                    "thread/attachment/list",
                    Some(json!({"threadId":requested_id})),
                )
                .await?;
            tokio::time::timeout(
                Duration::from_secs(10),
                client.read_stream_until_error_message(RequestId::Integer(id)),
            )
            .await?
        }
        .await;
        // Restore owned unreadable fixtures even when an RPC/assertion fails.
        std::fs::set_permissions(&path, permissions)?;
        let denied = result?;
        assert_eq!(denied.error.code, -32603, "{case}: {denied:?}");
        assert!(
            denied
                .error
                .message
                .starts_with("failed to read thread: thread-store internal error: "),
            "{case}: {denied:?}"
        );
        match case {
            "mismatch" => assert!(
                denied
                    .error
                    .message
                    .ends_with(&format!(", expected {requested_id}")),
                "{denied:?}"
            ),
            "io" => assert!(
                denied.error.message.contains("Permission denied"),
                "{denied:?}"
            ),
            _ => assert!(
                denied
                    .error
                    .message
                    .contains("failed to read session metadata"),
                "{denied:?}"
            ),
        }
    }
    assert!(server.received_requests().await.unwrap().is_empty());
    client.shutdown_gracefully().await?;
    Ok(())
}
