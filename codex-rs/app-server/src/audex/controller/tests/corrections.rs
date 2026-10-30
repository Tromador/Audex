//! Capacity, evidence and observation health are independent durable facts.
use super::*;

async fn release(
    controller: &AuditController,
    id: &str,
    revision: i64,
    acknowledge: bool,
) -> Result<Value, JSONRPCErrorError> {
    api(controller,"audit/release-slot",json!({"runId":id,"expectedRevision":revision,"reason":"Original runtime cannot be observed; operator accepts overlap","acknowledgePossibleConcurrentExecutionAndCharges":acknowledge})).await
}

#[tokio::test]
async fn unresolved_execution_requires_explicit_release_and_retains_late_completion()
-> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let (controller, fake, _) = fixture(&temp).await;
    let first = start(&controller, "lost-runtime").await;
    controller.drive(&first).await?;
    let store = controller.store().await?;
    let original = store.load_run(&first).await?;
    let saved = fake
        .work
        .lock()
        .expect("work")
        .remove(&original.operation_id)
        .expect("accepted work");
    controller.drive(&first).await?;
    api(&controller, "audit/cancel", json!({"runId":first}))
        .await
        .expect("cancel intent");
    controller.drive(&first).await?;
    let unresolved = store.load_run(&first).await?;
    assert_eq!(unresolved.operation_state, OperationState::Accepted);
    assert!(unresolved.observation_pending);
    assert!(!unresolved.capacity_released);
    let second = start(&controller, "new-independent-run").await;
    sqlx::query("UPDATE operations SET lease_expires_at_ms=0 WHERE id=?")
        .bind(&original.operation_id)
        .execute(&store.pool_for_tests())
        .await?;
    controller.drive(&second).await?;
    assert_eq!(
        fake.starts.load(Ordering::SeqCst),
        1,
        "cancellation and missing runtime retain capacity"
    );
    assert!(
        release(&controller, &first, unresolved.revision, false)
            .await
            .is_err()
    );
    let (competing, _, _) = fixture(&temp).await;
    let (a, b) = tokio::join!(
        release(&controller, &first, unresolved.revision, true),
        release(&competing, &first, unresolved.revision, true)
    );
    assert_eq!(a.expect("release"), b.expect("idempotent release"));
    let released = store.load_run(&first).await?;
    assert_eq!(released.revision, unresolved.revision + 1);
    assert_eq!(released.operation_state, unresolved.operation_state);
    assert_eq!(released.assignment_state, unresolved.assignment_state);
    assert!(released.capacity_released && released.observation_pending);
    controller.drive(&second).await?;
    assert_eq!(fake.starts.load(Ordering::SeqCst), 2);
    // A successful observation clears health only, never the permanent capacity fence.
    let snapshot = controller
        .snapshots()
        .open(&SnapshotId::parse(released.snapshot_id.clone())?)
        .await?;
    controller
        .reconcile_observation(&released, Some(&snapshot), None, saved.clone())
        .await?;
    let healthy = store.load_run(&first).await?;
    assert!(!healthy.observation_pending);
    assert!(healthy.capacity_released);
    assert_ne!(
        store
            .claim_operation(&original.operation_id, "replacement-owner", 1, 2)
            .await?,
        ClaimOutcome::Acquired
    );
    assert!(
        !store
            .begin_submission(
                &original.operation_id,
                &controller.owner,
                chrono::Utc::now().timestamp_millis()
            )
            .await?
    );
    // The original result is collected even with a newer reviewer already running.
    fake.work
        .lock()
        .expect("work")
        .insert(original.operation_id.clone(), saved);
    fake.replace(
        &original.operation_id,
        WorkState::Completed(empty_result(&original)),
    );
    controller.drive(&first).await?;
    controller.drive(&first).await?;
    assert_eq!(store.load_run(&first).await?.phase, RunPhase::Completed);
    assert!(store.load_run(&first).await?.capacity_released);
    assert_eq!(
        store.load_run(&second).await?.operation_state,
        OperationState::Accepted
    );
    for _ in 0..3 {
        controller.drive(&first).await?;
    }
    assert_eq!(
        fake.starts.load(Ordering::SeqCst),
        2,
        "neither recovery nor release repeats inference"
    );
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_capacity_releases")
        .fetch_one(&store.pool_for_tests())
        .await?;
    assert_eq!(rows, 1);
    let reopened = AuditStore::open(controller.root.clone()).await?;
    assert!(reopened.load_run(&first).await?.capacity_released);
    Ok(())
}

#[tokio::test]
async fn snapshot_outage_preserves_execution_capacity_and_recovers_without_dispatch()
-> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let (controller, fake, _) = fixture(&temp).await;
    let id = start(&controller, "snapshot-outage").await;
    controller.drive(&id).await?;
    let store = controller.store().await?;
    let run = store.load_run(&id).await?;
    let path = controller
        .root
        .join("snapshots")
        .join(SnapshotId::parse(run.snapshot_id.clone())?.digest());
    let backup = controller
        .root
        .join("snapshots")
        .join("temporarily-unavailable-snapshot");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            controller.root.join("snapshots").as_path(),
            std::fs::Permissions::from_mode(0o755),
        )?;
    }
    std::fs::rename(path.as_path(), backup.as_path())?;
    controller.drive(&id).await?;
    let waiting = store.load_run(&id).await?;
    assert_eq!(waiting.operation_state, OperationState::Accepted);
    assert!(waiting.observation_pending && !waiting.capacity_released);
    let diagnostic: String = sqlx::query_scalar("SELECT error FROM operations WHERE id=?")
        .bind(&run.operation_id)
        .fetch_one(&store.pool_for_tests())
        .await?;
    assert_eq!(diagnostic, "snapshot_access_unavailable");
    let second=api(&controller,"audit/start",json!({"requestKey":"different-source-outage", "source":{"type":"snippet","name":"different.rs","content":"fn other() {}\n"},"brief":"Static audit", "provider":controller.config.model_provider_id,"model":"audit-test"})).await.expect("independent source")["run"]["runId"].as_str().expect("ID").to_owned();
    controller.drive(&second).await?;
    assert_eq!(fake.starts.load(Ordering::SeqCst), 1);
    // Even a completed result waits for source verification, rather than being lost.
    fake.replace(&run.operation_id, WorkState::Completed(empty_result(&run)));
    controller.drive(&id).await?;
    assert_eq!(
        store.load_run(&id).await?.operation_state,
        OperationState::Accepted
    );
    std::fs::rename(backup.as_path(), path.as_path())?;
    controller.drive(&id).await?;
    controller.drive(&id).await?;
    assert_eq!(store.load_run(&id).await?.phase, RunPhase::Completed);
    assert!(!store.load_run(&id).await?.observation_pending);
    assert_eq!(fake.starts.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn healthy_observation_clears_pending_idempotently_and_rejects_stale_release()
-> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let (controller, fake, _) = fixture(&temp).await;
    let id = start(&controller, "health-recovery").await;
    controller.drive(&id).await?;
    let store = controller.store().await?;
    let run = store.load_run(&id).await?;
    let saved = fake
        .work
        .lock()
        .expect("work")
        .insert(
            run.operation_id.clone(),
            Observation::Deferred("temporary observation failure".into()),
        )
        .expect("work");
    controller.drive(&id).await?;
    let bad = store.load_run(&id).await?;
    assert!(bad.observation_pending);
    fake.work
        .lock()
        .expect("work")
        .insert(run.operation_id.clone(), saved);
    controller.drive(&id).await?;
    let healthy = store.load_run(&id).await?;
    assert!(!healthy.observation_pending);
    assert_eq!(healthy.revision, bad.revision + 1);
    controller.drive(&id).await?;
    assert_eq!(store.load_run(&id).await?.revision, healthy.revision);
    assert!(release(&controller, &id, bad.revision, true).await.is_err());
    assert!(!store.load_run(&id).await?.capacity_released);
    assert_eq!(fake.starts.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn completion_before_release_preserves_terminal_outcome_without_releasing_capacity()
-> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let (controller, fake, _) = fixture(&temp).await;
    let id = start(&controller, "completion-before-release").await;
    controller.drive(&id).await?;
    let store = controller.store().await?;
    let run = store.load_run(&id).await?;
    assert!(release(&controller, &id, run.revision, true).await.is_err());
    store
        .record_observation_issue(&run.operation_id, run.revision, "lost observation")
        .await?;
    let pending = store.load_run(&id).await?;
    assert!(
        release(&controller, &id, pending.revision, true)
            .await
            .is_err()
    );
    api(&controller, "audit/cancel", json!({"runId":id}))
        .await
        .expect("cancel intent");
    let consent = store.load_run(&id).await?;
    fake.replace(&run.operation_id, WorkState::Completed(empty_result(&run)));
    controller.drive(&id).await?;
    let completed = store.load_run(&id).await?;
    assert_eq!(completed.phase, RunPhase::Reporting);
    release(&controller, &id, consent.revision, true)
        .await
        .expect("completion supersedes release");
    let after = store.load_run(&id).await?;
    assert_eq!(after.revision, completed.revision);
    assert!(!after.capacity_released);
    assert!(!after.observation_pending);
    controller.drive(&id).await?;
    assert_eq!(store.load_run(&id).await?.phase, RunPhase::Completed);
    assert_eq!(fake.starts.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn release_while_completion_observation_is_suspended_retains_both_audits()
-> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let (controller, fake, _) = fixture(&temp).await;
    let id = start(&controller, "release-completion-race").await;
    controller.drive(&id).await?;
    let store = controller.store().await?;
    let run = store.load_run(&id).await?;
    store
        .record_observation_issue(&run.operation_id, run.revision, "lost observation")
        .await?;
    api(&controller, "audit/cancel", json!({"runId":id}))
        .await
        .expect("cancel");
    let waiting = store.load_run(&id).await?;
    fake.replace(&run.operation_id, WorkState::Completed(empty_result(&run)));
    fake.pause_interrupt.store(true, Ordering::SeqCst);
    let (driver, run_id) = (controller.clone(), id.clone());
    let task = tokio::spawn(async move { driver.drive(&run_id).await });
    fake.interrupt_entered.notified().await;
    release(&controller, &id, waiting.revision, true)
        .await
        .expect("release does not wait for observation");
    let next = start(&controller, "after-explicit-release").await;
    controller.drive(&next).await?;
    fake.interrupt_release.notify_one();
    task.await??;
    controller.drive(&id).await?;
    assert_eq!(store.load_run(&id).await?.phase, RunPhase::Completed);
    assert!(store.load_run(&id).await?.capacity_released);
    assert_eq!(
        store.load_run(&next).await?.operation_state,
        OperationState::Accepted
    );
    assert_eq!(fake.starts.load(Ordering::SeqCst), 2);
    // Completion first is also safe: release becomes a no-op, never a new state.
    let completed = store.load_run(&id).await?;
    release(&controller, &id, waiting.revision, true)
        .await
        .expect("repeat after completion");
    assert_eq!(store.load_run(&id).await?.revision, completed.revision);
    Ok(())
}

#[tokio::test]
async fn stalled_released_observation_cannot_permanently_block_worker_dispatch()
-> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let (controller, fake, _) = fixture(&temp).await;
    let id = start(&controller, "stalled-released").await;
    controller.drive(&id).await?;
    let store = controller.store().await?;
    let run = store.load_run(&id).await?;
    fake.work.lock().expect("work").remove(&run.operation_id);
    controller.drive(&id).await?;
    api(&controller, "audit/cancel", json!({"runId":id}))
        .await
        .expect("cancel");
    let pending = store.load_run(&id).await?;
    release(&controller, &id, pending.revision, true)
        .await
        .expect("release");
    let next = start(&controller, "after-stalled-observer").await;
    fake.pause_interrupt.store(true, Ordering::SeqCst);
    let driver = controller.clone();
    let task = tokio::spawn(async move { driver.tick().await });
    fake.interrupt_entered.notified().await;
    tokio::time::pause();
    tokio::time::advance(OBSERVATION_TIMEOUT + std::time::Duration::from_secs(1)).await;
    tokio::time::resume();
    task.await??;
    assert_eq!(
        store.load_run(&next).await?.operation_state,
        OperationState::Accepted
    );
    let original = store.load_run(&id).await?;
    assert_eq!(original.operation_state, OperationState::Accepted);
    assert!(original.capacity_released && original.observation_pending);
    assert_eq!(fake.starts.load(Ordering::SeqCst), 2);
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn integrity_failure_does_not_terminate_live_execution_and_cancel_remains_available()
-> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir()?;
    let (controller, fake, _) = fixture(&temp).await;
    let id = start(&controller, "integrity-active").await;
    controller.drive(&id).await?;
    let store = controller.store().await?;
    let run = store.load_run(&id).await?;
    let root = controller
        .root
        .join("snapshots")
        .join(SnapshotId::parse(run.snapshot_id.clone())?.digest());
    std::fs::set_permissions(root.as_path(), std::fs::Permissions::from_mode(0o755))?;
    // Invalid immutable root metadata is an integrity failure, not a transient absence.
    controller.drive(&id).await?;
    assert!(store.load_run(&id).await?.observation_pending);
    assert_eq!(
        store.load_run(&id).await?.operation_state,
        OperationState::Accepted
    );
    let diagnostic: String = sqlx::query_scalar("SELECT error FROM operations WHERE id=?")
        .bind(&run.operation_id)
        .fetch_one(&store.pool_for_tests())
        .await?;
    assert_eq!(diagnostic, "snapshot_integrity_failure");
    api(&controller, "audit/cancel", json!({"runId":id}))
        .await
        .expect("cancel");
    controller.drive(&id).await?;
    assert_eq!(
        store.load_run(&id).await?.phase,
        RunPhase::Cancelled,
        "only positive interruption ends execution"
    );
    assert_eq!(fake.starts.load(Ordering::SeqCst), 1);
    Ok(())
}
