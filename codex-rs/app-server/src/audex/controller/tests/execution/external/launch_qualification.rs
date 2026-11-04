//! Force the real bridge into the pre-identity startup window.
use super::*;

#[test]
fn early_bridge_waits_for_identity_or_stops_before_prompt_on_control() -> anyhow::Result<()> {
    run_test_with_large_stack("c63-early-bridge", || async {
        for cancel in [false, true] {
            let temp = tempfile::tempdir()?;
            let server = responses::start_mock_server().await;
            let entered = Arc::new(tokio::sync::Notify::new());
            let release = Arc::new(tokio::sync::Semaphore::new(0));
            let notify = entered.clone();
            let barrier = release.clone();
            let hooks = crate::audex::dispatch::DispatchTestHooks {
                checkpoint: Some(Arc::new(move |stage| {
                    let notify = notify.clone();
                    let barrier = barrier.clone();
                    Box::pin(async move {
                        if stage == "child_spawned" {
                            notify.notify_one();
                            let _permit = barrier.acquire().await;
                        }
                    })
                })),
                ..Default::default()
            };
            let (controller, threads) = fixture_with_hooks(
                &temp,
                &server.uri(),
                CodexAuth::from_api_key("local-dummy"),
                false,
                Arc::new(|_, _| Box::pin(async { Ok(()) })),
                hooks,
            )
            .await;
            let id = seed_external(&controller, &temp, "early").await?;
            std::fs::write(temp.path().join("early-bridge"), b"enabled")?;
            let running = controller.clone();
            let run_id = id.clone();
            let launch = tokio::spawn(async move { running.drive(&run_id).await });
            tokio::time::timeout(Duration::from_secs(10), entered.notified()).await?;
            let directory = launch_directory(&controller, &id).await?;
            let early = receipt(&directory, "early-bridge.json").await?;
            assert_eq!(early["identity_pending"], true);
            let store = controller.store().await?;
            let run = store.load_run(&id).await?;
            let pending = store.client_launch(&run.operation_id).await?.unwrap();
            assert!(pending.child_identity().is_none());
            assert!(pending.prompt_write_started_at_ms.is_none());
            assert!(!directory.join("receipt.json").exists());
            if cancel {
                api(&controller, "audit/attempt/control", json!({"runId":id,"assignmentId":run.assignment_id,
                    "actionKey":"early-cancel","action":"cancel","reason":"early bridge qualification"})).await.expect("durable cancel");
            }
            release.add_permits(1);
            launch.await??;
            if cancel {
                assert!(
                    store
                        .client_launch(&run.operation_id)
                        .await?
                        .unwrap()
                        .prompt_write_started_at_ms
                        .is_none()
                );
                assert!(!directory.join("receipt.json").exists());
            } else {
                let receipt = receipt(&directory, "receipt.json").await?;
                assert_eq!(receipt["prompt_count"], 1);
                assert_eq!(receipt["transcript"].as_array().unwrap().len(), 6);
                controller
                    .dispatch
                    .request_interrupt(run.operation_id.clone())
                    .await;
            }
            assert_eq!(
                std::fs::read_to_string(directory.join("spawn-count"))?
                    .lines()
                    .count(),
                1
            );
            threads
                .shutdown_all_threads_bounded(Duration::from_secs(5))
                .await;
            println!(
                "C63 F1: actual MCP bridge reached identity wait before persistence; cancel={cancel}; no unverified source service or prompt replay"
            );
        }
        Ok(())
    })
}
