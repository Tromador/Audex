use super::*;

#[test]
fn typed_tui_rosters_disclose_and_reject_every_unavailable_position() -> anyhow::Result<()> {
    run_test_with_large_stack("tui-typed-selections", || async {
        rejection_scenario()
            .await
            .map_err(|error| anyhow::anyhow!("{error:#}"))
    })
}

async fn rejection_scenario() -> Result<()> {
    let provider = responses::start_mock_server().await;
    let (home, _config, session, thread) = boxed_future!(fixture(&provider)).await?;
    let audits = AuditClient::new(session.request_handle());
    for (executor, route) in [
        ("claude_client", "claude_subscription"),
        ("claude_client", "claude_api"),
        ("native", "ollama_local"),
    ] {
        for position in 0..3 {
            let reviewers: Vec<Value> = (0..3)
                .map(|index| {
                    let mut selection = json!({"provider":"mock_provider", "model":"audit-test"});
                    if index == position {
                        selection["executionAdapter"] = json!(executor);
                        selection["authenticationRoute"] = json!(route);
                    }
                    selection
                })
                .collect();
            let action = tool_action(
                "audit_start",
                json!({
                    "source":{"type":"snippet", "name":"x.rs", "content":"source"},
                    "reviewers":reviewers, "scheduling":"sequential"
                }),
            )
            .map_err(color_eyre::eyre::Report::msg)?;
            let (tx, mut receive) = mpsc::unbounded_channel();
            let task = tokio::spawn(execute(
                session.request_handle(),
                thread,
                action,
                AppEventSender::new(tx),
            ));
            let error = tokio::time::timeout(Duration::from_secs(10), task)
                .await??
                .unwrap_err();
            assert!(error.contains("unavailable"), "{error}");
            assert!(
                receive.recv().await.is_none(),
                "unavailable roster must not reach confirmation"
            );
            assert!(provider.received_requests().await.unwrap().is_empty());
            assert!(
                audits
                    .list(AuditListParams {
                        after: None,
                        limit: None
                    })
                    .await?
                    .runs
                    .is_empty()
            );
            assert!(provider.received_requests().await.unwrap().is_empty());
            assert!(!home.0.path().join("audex/snapshots").exists());
        }
    }
    session.shutdown().await?;
    Ok(())
}
