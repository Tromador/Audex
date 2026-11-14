//! Durable authority races use independent store connections, with real committed prefixes.
use super::*;
use crate::audex::store::activity::ActivityInput;
use crate::audex::store::admission::AdmissionIdentity;
use crate::audex::store::attempts::{AttemptAction, ControlRequest};

async fn identity(
    controller: &AuditController,
    attempt: &AuditRunRecord,
) -> anyhow::Result<AdmissionIdentity> {
    let store = controller.store().await?;
    let (execution, _, _) = store
        .execution(&attempt.operation_id)
        .await?
        .expect("execution");
    Ok(AdmissionIdentity {
        run_id: attempt.run_id.clone(),
        assignment_id: attempt.assignment_id.clone(),
        operation_id: attempt.operation_id.clone(),
        execution_id: execution,
        owner: controller.owner.clone(),
        owner_generation: attempt.owner_generation,
        run_stop_generation: store.run_stop_generation(&attempt.run_id).await?,
    })
}

fn cancel(attempt: &AuditRunRecord, key: &str) -> ControlRequest {
    ControlRequest {
        run_id: attempt.run_id.clone(),
        assignment_id: attempt.assignment_id.clone(),
        action_key: key.to_owned(),
        action: AttemptAction::Cancel,
        reason: "fixture decision".to_owned(),
        authorise_retry: false,
        acknowledge_possible_overlap: false,
    }
}

#[tokio::test]
async fn competing_store_admission_and_stop_have_one_durable_order_without_sibling_revision_churn()
-> anyhow::Result<()> {
    for _ in 0..20 {
        let (_temp, controller, fake, id, attempts) = super::attempts::setup().await?;
        let capture = super::attempts::capture(&controller, &fake, &attempts[0]).await?;
        let store = controller.store().await?;
        store
            .append_activity(
                capture.identity(),
                &[ActivityInput {
                    producer_key: "retained".to_owned(),
                    turn_id: "turn".to_owned(),
                    item_id: "message".to_owned(),
                    kind: "assistant_delta".to_owned(),
                    payload: "unfinished {JSON".to_owned(),
                    text_offset: Some(0),
                }],
            )
            .await?;
        let owner = identity(&controller, &attempts[0]).await?;
        let other = AuditController::new(
            controller.config.clone(),
            controller.models.clone(),
            fake.clone(),
            Arc::new(|_| {}),
        );
        let request = json!({"runId":id,"assignmentId":attempts[0].assignment_id,"action":"cancel","actionKey":"stop-race","reason":"fixture decision","authoriseRetry":false,"acknowledgePossibleOverlap":false});
        let now = chrono::Utc::now().timestamp_millis();
        let (admitted, stopped) = tokio::join!(
            store.admit_generation(&owner, "raced-generation", "native_http", now),
            api(&other, "audit/attempt/control", request)
        );
        let stopped = stopped.expect("competing controller stop");
        assert_eq!(stopped["cutoff"], 1);
        let records = store.generation_records(&attempts[0].assignment_id).await?;
        assert_eq!(records.len(), usize::from(admitted.is_ok()));
        assert!(
            records
                .iter()
                .all(|record| record.send_started_at_ms.is_none()
                    && record.confirmed_at_ms.is_none())
        );
        assert!(
            !store
                .mark_generation_send(&owner, "raced-generation", now)
                .await?
        );
        assert!(
            store
                .admit_generation(&owner, "after-stop", "native_websocket", now)
                .await
                .is_err()
        );
        assert_eq!(
            store.activity(&attempts[0].assignment_id, 0, 64).await?[0].payload,
            "unfinished {JSON"
        );
        for peer in &attempts[1..] {
            fake.replace(&peer.operation_id, WorkState::Completed(empty_result(peer)));
            controller.drive_operation(&peer.operation_id).await?;
            let current = store
                .attempt_by_assignment(&id, &peer.assignment_id)
                .await?;
            assert_eq!(current.assignment_state, AssignmentState::Succeeded);
            assert_eq!(current.attempt_revision, peer.attempt_revision + 1);
            let collection_ready = peer.reviewer_ordinal == 2;
            assert_eq!(
                current.revision,
                peer.revision + i64::from(collection_ready)
            );
            assert_eq!(current.collection_ready, collection_ready);
            assert_eq!(
                current.phase,
                if collection_ready {
                    RunPhase::Reporting
                } else {
                    RunPhase::Reviewing
                }
            );
            if !collection_ready {
                let sibling = store
                    .attempt_by_assignment(&id, &attempts[2].assignment_id)
                    .await?;
                assert_eq!(sibling.attempt_revision, attempts[2].attempt_revision);
            }
        }
    }
    Ok(())
}

#[tokio::test]
async fn lost_admission_acknowledgement_and_restart_never_reissue_the_original_request()
-> anyhow::Result<()> {
    let (_temp, controller, fake, id, attempts) = super::attempts::setup().await?;
    let _capture = super::attempts::capture(&controller, &fake, &attempts[0]).await?;
    let store = controller.store().await?;
    let owner = identity(&controller, &attempts[0]).await?;
    let now = chrono::Utc::now().timestamp_millis();
    // The process loses its acknowledgement immediately after this commit.
    store
        .admit_generation(&owner, "lost-ack", "native_http", now)
        .await?;
    let before = store.generation_records(&attempts[0].assignment_id).await?;
    assert!(before[0].send_started_at_ms.is_none() && before[0].confirmed_at_ms.is_none());
    let reopened = AuditStore::open(controller.root.clone()).await?;
    assert!(
        reopened
            .admit_generation(&owner, "lost-ack", "native_http", now)
            .await
            .is_err()
    );
    assert_eq!(
        reopened
            .generation_records(&attempts[0].assignment_id)
            .await?,
        before
    );
    assert_eq!(
        reopened
            .attempt_by_assignment(&id, &attempts[0].assignment_id)
            .await?
            .assignment_state,
        AssignmentState::Running
    );
    sqlx::query("UPDATE operations SET lease_expires_at_ms=0 WHERE id=?")
        .bind(&owner.operation_id)
        .execute(&store.pool_for_tests())
        .await?;
    let recovered_dispatch = Arc::new(Fake::default());
    *recovered_dispatch.root.lock().expect("root") = Some(controller.root.clone());
    let recovered = AuditController::new(
        controller.config.clone(),
        controller.models.clone(),
        recovered_dispatch.clone(),
        Arc::new(|_| {}),
    );
    recovered.tick().await?;
    assert_eq!(
        recovered_dispatch.starts.load(Ordering::SeqCst),
        0,
        "restart only inspects existing executions"
    );
    assert_eq!(
        reopened
            .generation_records(&attempts[0].assignment_id)
            .await?,
        before
    );
    assert_eq!(
        fake.starts.load(Ordering::SeqCst),
        3,
        "inspection never replays a generation"
    );
    Ok(())
}

#[tokio::test]
async fn stale_owner_execution_generation_and_expired_lease_cannot_admit_or_renew()
-> anyhow::Result<()> {
    let (_temp, controller, fake, _id, attempts) = super::attempts::setup().await?;
    let _capture = super::attempts::capture(&controller, &fake, &attempts[0]).await?;
    let store = controller.store().await?;
    let owner = identity(&controller, &attempts[0]).await?;
    let now = chrono::Utc::now().timestamp_millis();
    for changed in 0..4 {
        let mut stale = owner.clone();
        match changed {
            0 => stale.owner = "other-controller".to_owned(),
            1 => stale.owner_generation += 1,
            2 => stale.execution_id = "other-execution".to_owned(),
            _ => stale.run_stop_generation += 1,
        }
        assert!(
            store
                .admit_generation(&stale, "stale", "native_http", now)
                .await
                .is_err()
        );
        assert!(!store.renew_execution_lease(&stale, now).await?);
    }
    assert!(store.renew_execution_lease(&owner, now).await?);
    sqlx::query("UPDATE operations SET lease_expires_at_ms=? WHERE id=?")
        .bind(now)
        .bind(&owner.operation_id)
        .execute(&store.pool_for_tests())
        .await?;
    assert!(
        !store.renew_execution_lease(&owner, now + 1).await?,
        "expiry cannot be resurrected"
    );
    assert!(
        !store.renew_execution_lease(&owner, now - 1000).await?,
        "a timestamp captured before a writer wait cannot renew an expired owner"
    );
    assert!(
        store
            .admit_generation(&owner, "stale-clock", "native_http", now - 1000)
            .await
            .is_err()
    );
    assert!(
        store
            .admit_generation(&owner, "expired", "native_websocket", now + 1)
            .await
            .is_err()
    );
    assert!(
        store
            .generation_records(&owner.assignment_id)
            .await?
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn whole_run_stop_fences_every_attempt_and_preserves_completed_sibling_evidence()
-> anyhow::Result<()> {
    let (_temp, controller, fake, id, attempts) = super::attempts::setup().await?;
    let store = controller.store().await?;
    let mut owners = Vec::new();
    for attempt in &attempts {
        let _capture = super::attempts::capture(&controller, &fake, attempt).await?;
        owners.push(identity(&controller, attempt).await?);
    }
    fake.replace(
        &attempts[2].operation_id,
        WorkState::Completed(empty_result(&attempts[2])),
    );
    controller
        .drive_operation(&attempts[2].operation_id)
        .await?;
    let completed = store
        .attempt_by_assignment(&id, &attempts[2].assignment_id)
        .await?;
    store.request_cancel(&id, completed.revision).await?;
    assert_eq!(
        store.run_stop_generation(&id).await?,
        owners[0].run_stop_generation + 1
    );
    for (ordinal, owner) in owners.iter().enumerate() {
        assert!(
            store
                .admit_generation(
                    owner,
                    &format!("post-abort-{ordinal}"),
                    "native_http",
                    chrono::Utc::now().timestamp_millis()
                )
                .await
                .is_err()
        );
    }
    let retained = store
        .attempt_by_assignment(&id, &completed.assignment_id)
        .await?;
    assert_eq!(retained.assignment_state, AssignmentState::Succeeded);
    assert_eq!(
        store.raw_result(&retained)?,
        Some(empty_result(&attempts[2]))
    );
    Ok(())
}

#[tokio::test]
async fn provider_receipt_after_stop_is_operational_only_and_replay_is_refused()
-> anyhow::Result<()> {
    let (_temp, controller, fake, _id, attempts) = super::attempts::setup().await?;
    let _capture = super::attempts::capture(&controller, &fake, &attempts[0]).await?;
    let store = controller.store().await?;
    let owner = identity(&controller, &attempts[0]).await?;
    let now = chrono::Utc::now().timestamp_millis();
    store
        .admit_generation(&owner, "sent-before-stop", "native_websocket", now)
        .await?;
    assert!(
        store
            .mark_generation_send(&owner, "sent-before-stop", now)
            .await?
    );
    assert!(
        store
            .mark_generation_send(&owner, "sent-before-stop", now)
            .await?
    );
    let stopped = store
        .control_attempt(&cancel(&attempts[0], "stop"), None)
        .await?;
    let frozen = store.capture_state(&owner.assignment_id).await?;
    assert!(
        !store
            .mark_generation_send(&owner, "sent-before-stop", now)
            .await?
    );
    store
        .confirm_generation(
            &owner,
            "sent-before-stop",
            Some("provider-response"),
            now + 1,
        )
        .await?;
    store
        .confirm_generation(
            &owner,
            "sent-before-stop",
            Some("provider-response"),
            now + 2,
        )
        .await?;
    let records = store.generation_records(&owner.assignment_id).await?;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].confirmed_at_ms, Some(now + 1));
    assert_eq!(store.capture_state(&owner.assignment_id).await?, frozen);
    assert_eq!(stopped.disposition, "cancelled");
    assert!(
        store
            .admit_generation(&owner, "sent-before-stop", "native_websocket", now + 2)
            .await
            .is_err()
    );
    assert!(
        store
            .confirm_generation(&owner, "sent-before-stop", Some("wrong-response"), now + 2)
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn committed_run_stop_is_not_relabelled_failed_by_late_admission_refusal()
-> anyhow::Result<()> {
    let (_temp, controller, _fake, id, attempts) = super::attempts::setup().await?;
    let store = controller.store().await?;
    store.request_cancel(&id, attempts[0].revision).await?;
    assert!(matches!(
        store
            .finish_operation(
                &attempts[0].operation_id,
                attempts[0].attempt_revision,
                AssignmentState::Failed,
                "generation admission refused",
            )
            .await,
        Err(crate::audex::store::AuditStoreError::RevisionConflict { .. })
    ));
    let stopped = store
        .attempt_by_assignment(&id, &attempts[0].assignment_id)
        .await?;
    assert_eq!(stopped.attempt_revision, attempts[0].attempt_revision + 1);
    assert_eq!(stopped.control_intent, "cancel_requested");
    assert_eq!(stopped.phase, RunPhase::Reporting);
    store
        .finish_operation(
            &stopped.operation_id,
            stopped.attempt_revision,
            AssignmentState::Failed,
            "generation admission refused",
        )
        .await?;
    assert_eq!(
        store
            .attempt_by_assignment(&id, &attempts[0].assignment_id)
            .await?
            .assignment_state,
        AssignmentState::Cancelled
    );
    Ok(())
}
