//! Slice 1: deterministic concurrency, attribution, and cold reconstruction.
use super::*;
use std::time::Duration;

pub(super) async fn roster(
    controller: &AuditController,
    key: &str,
    count: usize,
) -> anyhow::Result<String> {
    let params: AuditStartParams = serde_json::from_value(json!({
        "requestKey":key,"source":{"type":"snippet","name":"example.rs","content":"fn main() {}\n"},
        "brief":"Static correctness audit", "provider":controller.config.model_provider_id,"model":"audit-test"
    }))?;
    let selections = (0..count)
        .map(|ordinal| ReviewerSelection {
            provider: controller.config.model_provider_id.clone(),
            model: format!("audit-reviewer-{ordinal}"),
            reasoning_effort: None,
        })
        .collect();
    Ok(controller
        .commission_roster(params, selections, &mut None)
        .await?
        .run_id)
}

fn concurrent_controller(original: &AuditController, fake: &Arc<Fake>) -> Arc<AuditController> {
    AuditController::new(
        original.config.clone(),
        original.models.clone(),
        fake.clone(),
        Arc::new(|_| {}),
    )
}

#[tokio::test]
async fn three_attempts_progress_without_sibling_revision_invalidation_and_reconstruct()
-> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let (original, fake, _) = fixture(&temp).await;
    let controller = concurrent_controller(&original, &fake);
    let id = roster(&controller, "three-independent", 3).await?;
    let store = controller.store().await?;
    let before = store.attempts(&id).await?;
    assert_eq!(before.len(), 3);
    assert_eq!(
        before
            .iter()
            .map(|a| &a.reviewer_id)
            .collect::<std::collections::HashSet<_>>()
            .len(),
        3
    );
    assert_eq!(
        before
            .iter()
            .map(|a| &a.assignment_id)
            .collect::<std::collections::HashSet<_>>()
            .len(),
        3
    );
    assert_eq!(
        before
            .iter()
            .map(|a| &a.operation_id)
            .collect::<std::collections::HashSet<_>>()
            .len(),
        3
    );
    for (ordinal, attempt) in before.iter().enumerate() {
        assert_eq!(attempt.snapshot_id, before[0].snapshot_id);
        assert_eq!(attempt.run_id, id);
        assert_eq!(attempt.reviewer_ordinal, ordinal as i64);
        assert_eq!(attempt.attempt_ordinal, 1);
        assert_eq!(attempt.provider, controller.config.model_provider_id);
        assert_eq!(attempt.model, format!("audit-reviewer-{ordinal}"));
        let (request, policy, policy_digest, _) =
            store.attempt_input(&attempt.operation_id).await?;
        assert_eq!(sha256_hex(policy.as_bytes()), policy_digest);
        assert_eq!(
            serde_json::from_str::<Value>(&request)?["model"],
            attempt.model
        );
    }
    // A barrier proves all three cross start before any acceptance is returned.
    fake.pause_start.store(true, Ordering::SeqCst);
    let task_controller = controller.clone();
    let task = tokio::spawn(async move { task_controller.tick().await });
    tokio::time::timeout(Duration::from_secs(3), async {
        while fake.starts.load(Ordering::SeqCst) < 3 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("all three starts enter independently");
    assert_eq!(fake.starts.load(Ordering::SeqCst), 3);
    for _ in 0..3 {
        fake.release.notify_one();
        tokio::task::yield_now().await;
    }
    fake.pause_start.store(false, Ordering::SeqCst);
    // Notify stores one permit; release all remaining waiters as well.
    fake.release.notify_waiters();
    task.await??;
    let running = store.attempts(&id).await?;
    assert!(
        running
            .iter()
            .all(|a| a.assignment_state == AssignmentState::Running)
    );
    assert!(
        running
            .iter()
            .all(|a| a.attempt_revision == 3 && a.owner_generation == 1)
    );
    let run_revision = running[0].revision;
    let snapshot = controller
        .snapshots()
        .open(&SnapshotId::parse(running[0].snapshot_id.clone())?)
        .await?;
    // Use observations taken before any sibling write. One failed reviewer does
    // not discard another's valid result or invalidate the third's live observation.
    let first = Observation::Accepted {
        operation_id: running[0].operation_id.clone(),
        snapshot_id: running[0].snapshot_id.clone(),
        receipt: "first".to_owned(),
        state: WorkState::Completed(empty_result(&running[0])),
    };
    let second = Observation::Accepted {
        operation_id: running[1].operation_id.clone(),
        snapshot_id: running[1].snapshot_id.clone(),
        receipt: "second".to_owned(),
        state: WorkState::Failed("fake failure".to_owned()),
    };
    let (a, b, c) = tokio::join!(
        controller.reconcile_observation(&running[0], Some(&snapshot), None, first),
        controller.reconcile_observation(&running[1], Some(&snapshot), None, second),
        store.record_observation_issue(
            &running[2].operation_id,
            running[2].attempt_revision,
            "fake disconnected"
        )
    );
    a?;
    b?;
    c?;
    let durable = store.attempts(&id).await?;
    assert_eq!(durable[0].assignment_state, AssignmentState::Succeeded);
    assert_eq!(durable[1].assignment_state, AssignmentState::Failed);
    assert_eq!(durable[2].assignment_state, AssignmentState::Running);
    assert!(durable[2].observation_pending);
    assert!(
        durable
            .iter()
            .all(|a| a.revision == run_revision && a.attempt_revision == 4)
    );
    assert!(durable[0].result_digest.is_some());
    assert!(durable[1].result_digest.is_none() && durable[2].result_digest.is_none());
    let raw = store
        .raw_result(&durable[0])?
        .expect("durable first result");
    assert_eq!(
        serde_json::from_slice::<Value>(&raw)?["assignment_id"],
        durable[0].assignment_id
    );
    // A cold store and controller reconstruct the identities and statuses exactly;
    // they observe the original work, never recreate or re-commission it.
    let root = controller.root.clone();
    let reopened = AuditStore::open(root).await?;
    assert_eq!(reopened.attempts(&id).await?, durable);
    let recovered = concurrent_controller(&controller, &fake);
    fake.work
        .lock()
        .expect("work")
        .remove(&durable[2].operation_id);
    recovered.tick().await?;
    let after = recovered.store().await?.attempts(&id).await?;
    assert_eq!(after[0], durable[0]);
    assert_eq!(after[1], durable[1]);
    assert_eq!(after[2].assignment_state, AssignmentState::Running);
    assert!(after[2].observation_pending && after[2].result_digest.is_none());
    assert_eq!(fake.starts.load(Ordering::SeqCst), 3);
    // Whole-run publication is deliberately outside the multi-review foundation.
    assert_eq!(after[0].phase, RunPhase::Reviewing);
    assert!(after[0].report.is_none());
    Ok(())
}

#[tokio::test]
async fn suspended_observation_does_not_block_peer_progress_or_scheduled_recovery()
-> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let (original, fake, _) = fixture(&temp).await;
    let controller = concurrent_controller(&original, &fake);
    let id = roster(&controller, "paused-peer", 3).await?;
    controller.tick().await?;
    let attempts = controller.store().await?.attempts(&id).await?;
    fake.pause_inspect.store(true, Ordering::SeqCst);
    let worker = controller.clone();
    let operation = attempts[0].operation_id.clone();
    let stalled = tokio::spawn(async move { worker.drive_operation(&operation).await });
    fake.inspect_entered.notified().await;
    fake.replace(
        &attempts[1].operation_id,
        WorkState::Completed(empty_result(&attempts[1])),
    );
    controller.recover_runs(true).await?;
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let peer = controller
                .store()
                .await?
                .run_for_operation(&attempts[1].operation_id)
                .await?;
            if peer.assignment_state == AssignmentState::Succeeded {
                break Ok::<_, anyhow::Error>(());
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("peer completes while first observation is suspended")?;
    fake.inspect_release.notify_one();
    stalled.await??;
    assert_eq!(fake.starts.load(Ordering::SeqCst), 3);
    controller.stop_new_dispatch();
    Ok(())
}

#[tokio::test]
async fn default_capacity_is_three_and_sequential_mode_uses_the_same_claims() -> anyhow::Result<()>
{
    let temp = tempfile::tempdir()?;
    let (original, fake, _) = fixture(&temp).await;
    let controller = concurrent_controller(&original, &fake);
    let id = roster(&controller, "bounded-roster", 4).await?;
    controller.tick().await?;
    assert_eq!(fake.starts.load(Ordering::SeqCst), 3);
    let attempts = controller.store().await?.attempts(&id).await?;
    assert_eq!(
        attempts
            .iter()
            .filter(|a| a.assignment_state == AssignmentState::Running)
            .count(),
        3
    );
    assert_eq!(attempts[3].assignment_state, AssignmentState::Planned);
    fake.replace(
        &attempts[0].operation_id,
        WorkState::Completed(empty_result(&attempts[0])),
    );
    controller
        .drive_operation(&attempts[0].operation_id)
        .await?;
    controller
        .drive_operation(&attempts[3].operation_id)
        .await?;
    assert_eq!(fake.starts.load(Ordering::SeqCst), 4);
    let temp = tempfile::tempdir()?;
    let (sequential, fake, _) = fixture(&temp).await;
    let id = roster(&sequential, "sequential-roster", 3).await?;
    sequential.tick().await?;
    assert_eq!(fake.starts.load(Ordering::SeqCst), 1);
    let attempts = sequential.store().await?.attempts(&id).await?;
    assert_eq!(attempts[1].assignment_state, AssignmentState::Planned);
    assert_eq!(attempts[2].assignment_state, AssignmentState::Planned);
    Ok(())
}
