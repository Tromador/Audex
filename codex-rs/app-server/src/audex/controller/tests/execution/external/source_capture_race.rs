//! Capture failure after the controller check must retain observed client acceptance.
use super::*;

#[test]
fn rejected_source_receipt_during_inspection_preserves_known_acceptance() -> anyhow::Result<()> {
    run_test_with_large_stack("source-capture-race", || {
        reject_receipt_at("before_inspect")
    })
}

#[test]
fn rejected_source_receipt_after_launch_snapshot_preserves_known_acceptance() -> anyhow::Result<()>
{
    run_test_with_large_stack("source-capture-snapshot-race", || {
        reject_receipt_at("client_launch_observed")
    })
}

async fn reject_receipt_at(checkpoint: &'static str) -> anyhow::Result<()> {
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
                if stage == checkpoint {
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
    let id = capture::seed_stream(&controller, &temp, "valid", false).await?;
    std::fs::write(
            temp.path().join("stream-scenario.json"),
            json!({"mode":"valid","gate":false,"gate_start":true,
                "source_receipt":{"type":"tool_result","content":[{"type":"text",
                    "text":"Error: result (192,013 characters) exceeds maximum allowed tokens. Output has been saved to /private/PRIVATE_SENTINEL_MUST_NOT_PERSIST."}]},
                "source_operation":"read",
                "source_arguments":{"path":"example.rs","offset":0,"max_bytes":65536}})
            .to_string(),
        )?;
    controller.drive(&id).await?;
    let store = controller.store().await?;
    let pending = store.load_run(&id).await?;
    assert!(
        store
            .client_capture_failure(&pending.assignment_id)
            .await?
            .is_none()
    );
    let driver = controller.clone();
    let run_id = id.clone();
    let inspecting = tokio::spawn(async move { driver.drive(&run_id).await });
    tokio::time::timeout(Duration::from_secs(10), entered.notified()).await?;
    // The controller has read a clean capture state. Let the real collector
    // observe acceptance and reject the receipt before inspection continues.
    std::fs::write(temp.path().join("before-public.release"), b"release")?;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if store
                .client_capture_failure(&pending.assignment_id)
                .await?
                .is_some()
            {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::task::yield_now().await;
        }
    })
    .await??;
    let before = store.load_run(&id).await?;
    let launch = store.client_launch(&before.operation_id).await?.unwrap();
    assert_eq!(
        before.operation_state,
        crate::audex::store::OperationState::Dispatching
    );
    assert!(launch.client_accepted_at_ms.is_some());
    release.add_permits(1);
    inspecting.await??;
    let run = capture::settled(&controller, &id).await?;
    assert_eq!(run.assignment_state, AssignmentState::Failed);
    let failure = store.client_capture_failure(&run.assignment_id).await?;
    assert_eq!(failure.as_deref(), Some("external_tool_result_invalid"));
    let activity = store.activity(&run.assignment_id, 0, 100).await?;
    assert!(
        !activity
            .iter()
            .any(|event| event.kind == "tool_result" && event.item_id == "call-1")
    );
    // Failed active assignments require the existing operator selection step.
    api(
        &controller,
        "audit/attempt/control",
        json!({
            "runId":id,"assignmentId":run.assignment_id,"actionKey":"exclude-failed",
            "action":"ignore","reason":"freeze rejected receipt for inspection"
        }),
    )
    .await
    .expect("exclude failed fixture after asserting its failure");
    let public = finished(&controller, &id).await;
    assert_ne!(public["outcome"], "success");
    let frozen = store.freeze_report(&id).await?;
    assert!(frozen.reviewers[0].opinion.is_none());
    assert!(
        frozen.reviewers[0]
            .source_tools
            .as_ref()
            .is_none_or(|tools| tools
                .calls
                .iter()
                .all(|call| call.call_id != "call-1" || call.outcome != Outcome::Success))
    );
    if let Ok(root) = std::env::var("AUDEX_FIDELITY_EVIDENCE") {
        let root = PathBuf::from(root)
            .join("capture-during-inspection")
            .join(checkpoint);
        std::fs::create_dir_all(&root)?;
        std::fs::write(
            root.join("collector.json"),
            json!({"assignment_state":format!("{:?}",run.assignment_state),
                    "operation_before_inspection":format!("{:?}",before.operation_state),
                    "client_acceptance_observed":launch.client_accepted_at_ms.is_some(),
                    "capture_failure":failure,"activity":activity,"report":public})
            .to_string(),
        )?;
    }
    capture::assert_no_private_files(controller.root.as_path())?;
    assert!(server.received_requests().await.unwrap().is_empty());
    threads
        .shutdown_all_threads_bounded(Duration::from_secs(5))
        .await;
    Ok(())
}
