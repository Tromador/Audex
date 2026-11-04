//! Exact controls at held production launch and terminal boundaries.
use super::capture::seed_stream;
use super::*;

#[test]
fn controls_at_launch_and_terminal_boundaries_never_replay_or_cross_attempts() -> anyhow::Result<()>
{
    run_test_with_large_stack("c63-control-boundaries", || async {
        for stage in [
            "launch_intent",
            "child_identity",
            "prompt_write_started",
            "terminal_persisted",
        ] {
            for action in ["cancel", "ignore", "retry", "abort"] {
                let temp = tempfile::tempdir()?;
                let server = responses::start_mock_server().await;
                let entered = Arc::new(tokio::sync::Notify::new());
                let release = Arc::new(tokio::sync::Semaphore::new(0));
                let once = Arc::new(AtomicBool::new(false));
                let (notify, barrier) = (entered.clone(), release.clone());
                let hooks = crate::audex::dispatch::DispatchTestHooks {
                    checkpoint: Some(Arc::new(move |name| {
                        let (notify, barrier, once) =
                            (notify.clone(), barrier.clone(), once.clone());
                        Box::pin(async move {
                            if name == stage && !once.swap(true, Ordering::SeqCst) {
                                notify.notify_one();
                                let _guard = barrier.acquire().await;
                            }
                        })
                    })),
                    ..Default::default()
                };
                let (controller, threads) = fixture_with_hooks(
                    &temp,
                    &server.uri(),
                    CodexAuth::from_api_key("dummy"),
                    false,
                    Arc::new(|_, _| Box::pin(async { Ok(()) })),
                    hooks,
                )
                .await;
                let id = seed_stream(&controller, &temp, "valid", false).await?;
                let driver = controller.clone();
                let run_id = id.clone();
                let task = tokio::spawn(async move { driver.drive(&run_id).await });
                tokio::time::timeout(Duration::from_secs(10), entered.notified()).await?;
                let store = controller.store().await?;
                let run = store.load_run(&id).await?;
                let launch = store.client_launch(&run.operation_id).await?.unwrap();
                let decision=tokio::time::timeout(Duration::from_secs(2),async {
                    if action=="abort" {controller.abort_audit(&id).await?;Ok(Value::Null)} else {
                        api(&controller,"audit/attempt/control",json!({"runId":id,"assignmentId":run.assignment_id,
                            "actionKey":format!("{stage}-{action}"),"action":action,"reason":"held launch qualification",
                            "authoriseRetry":action=="retry","acknowledgePossibleOverlap":true})).await.map_err(|e|anyhow::anyhow!("control: {e:?}"))
                    }
                }).await??;
                let prefix = store.activity(&run.assignment_id, 0, 100).await?;
                release.add_permits(1);
                if let Err(error) = task.await? {
                    // Controls do not acquire the observation gate. A driver
                    // already holding the old revision may be rejected by the
                    // writer after the control commits; normal recovery reinspects.
                    assert!(
                        matches!(error.downcast_ref::<crate::audex::store::AuditStoreError>(),
                        Some(crate::audex::store::AuditStoreError::RevisionConflict { expected, actual }) if actual > expected),
                        "unexpected driver failure: {error:#}"
                    );
                }
                super::lifecycle_qualification::reconcile_until(&controller, &run.operation_id)
                    .await?;
                assert_eq!(store.activity(&run.assignment_id, 0, 100).await?, prefix);
                assert_eq!(
                    store
                        .client_launch(&run.operation_id)
                        .await?
                        .unwrap()
                        .execution_id,
                    launch.execution_id
                );
                if stage != "terminal_persisted" && action != "retry" {
                    assert!(!temp.path().join("stream-prompt-receipt.json").exists());
                }
                if action == "retry" {
                    let replacement = decision["replacementOperationId"].as_str().unwrap();
                    assert_ne!(
                        store.execution(replacement).await?.unwrap().0,
                        launch.execution_id
                    );
                }
                if action == "ignore" {
                    assert!(api(&controller,"audit/attempt/control",json!({"runId":id,"assignmentId":run.assignment_id,
                        "actionKey":"forbidden-retry","action":"retry","reason":"ignored remains permanent",
                        "authoriseRetry":true,"acknowledgePossibleOverlap":true})).await.is_err());
                }
                threads
                    .shutdown_all_threads_bounded(Duration::from_secs(5))
                    .await;
                println!(
                    "C63 held {stage} + {action}: acknowledgement independent of held launch/collector; original prefix final; only explicit Retry replaces execution"
                );
            }
        }
        Ok(())
    })
}
