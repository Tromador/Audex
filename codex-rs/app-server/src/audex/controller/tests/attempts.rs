//! Controls race durable capture/completion, never runtime termination.
use super::*;
use crate::audex::capture::CaptureHandle;
use crate::audex::store::activity::{ActivityInput, CaptureIdentity};
use anyhow::Context;
use codex_protocol::protocol::{AgentMessageContentDeltaEvent, Event, EventMsg};
use std::time::Duration;

async fn setup() -> anyhow::Result<(
    tempfile::TempDir,
    Arc<AuditController>,
    Arc<Fake>,
    String,
    Vec<AuditRunRecord>,
)> {
    let temp = tempfile::tempdir()?;
    let (base, fake, _) = fixture(&temp).await;
    let controller = AuditController::new(
        base.config.clone(),
        base.models.clone(),
        fake.clone(),
        Arc::new(|_| {}),
    );
    let id = super::foundations::roster(&controller, "attempt-controls", 3).await?;
    controller.tick().await?;
    let attempts = controller.store().await?.attempts(&id).await?;
    Ok((temp, controller, fake, id, attempts))
}

async fn capture(
    controller: &AuditController,
    fake: &Fake,
    attempt: &AuditRunRecord,
) -> anyhow::Result<CaptureHandle> {
    let store = controller.store().await?;
    let thread = codex_protocol::ThreadId::new().to_string();
    sqlx::query("INSERT INTO audit_execution(operation_id,thread_id,policy_digest) VALUES(?,?,?)")
        .bind(&attempt.operation_id)
        .bind(&thread)
        .bind(store.attempt_input(&attempt.operation_id).await?.2)
        .execute(&store.pool_for_tests())
        .await?;
    let capture = CaptureHandle::new(
        store.clone(),
        CaptureIdentity {
            assignment_id: attempt.assignment_id.clone(),
            session_id: thread,
            owner_generation: attempt.owner_generation,
        },
        None,
    )
    .await?;
    fake.captures
        .lock()
        .expect("captures")
        .insert(attempt.operation_id.clone(), capture.clone());
    Ok(capture)
}

async fn delta(capture: &CaptureHandle, text: &str) {
    capture
        .observe(&Event {
            id: "turn".to_owned(),
            msg: EventMsg::AgentMessageContentDelta(AgentMessageContentDeltaEvent {
                thread_id: capture.identity().session_id.clone(),
                turn_id: "turn".to_owned(),
                item_id: "message".to_owned(),
                delta: text.to_owned(),
            }),
        })
        .await;
}

fn control(run: &str, assignment: &str, key: &str, action: &str) -> Value {
    json!({"runId":run,"assignmentId":assignment,"actionKey":key,"action":action,"reason":"fixture operator decision",
        "authoriseRetry":action=="retry","acknowledgePossibleOverlap":action=="retry"})
}

#[tokio::test]
async fn cancel_and_ignore_preserve_buffered_prefix_without_waiting_for_stop_and_do_not_touch_peers()
-> anyhow::Result<()> {
    for action in ["cancel", "ignore"] {
        let (_temp, controller, fake, id, attempts) = setup().await?;
        let store = controller.store().await?;
        let first = capture(&controller, &fake, &attempts[0]).await?;
        let held = first.hold_flush().await;
        let fragment = "{\"unfinished\": \"received before decision";
        delta(&first, fragment).await;
        assert!(
            store
                .activity(&attempts[0].assignment_id, 0, 64)
                .await?
                .is_empty(),
            "uncommitted material is not visible"
        );
        fake.pause_interrupt.store(true, Ordering::SeqCst);
        let decision = tokio::time::timeout(
            Duration::from_secs(2),
            api(
                &controller,
                "audit/attempt/control",
                control(&id, &attempts[0].assignment_id, "stop", action),
            ),
        )
        .await?
        .expect("decision");
        tokio::time::timeout(Duration::from_secs(2), fake.interrupt_entered.notified()).await?;
        assert_eq!(decision["cutoff"], 1);
        let state = store.capture_state(&attempts[0].assignment_id).await?;
        assert_eq!(state.cutoff, Some(1));
        assert_eq!(
            state.disposition,
            if action == "cancel" {
                "cancelled"
            } else {
                "ignored"
            }
        );
        assert!(
            store
                .attempt_by_assignment(&id, &attempts[0].assignment_id)
                .await?
                .capacity_released
        );
        let retained = store.activity(&attempts[0].assignment_id, 0, 64).await?;
        assert_eq!(retained.len(), 1);
        assert_eq!(retained[0].payload, fragment);
        drop(held);
        delta(&first, " LATE").await;
        let late = ActivityInput {
            producer_key: "late".to_owned(),
            turn_id: "turn".to_owned(),
            item_id: "message".to_owned(),
            kind: "assistant_delta".to_owned(),
            payload: "LATE".to_owned(),
            text_offset: Some(fragment.len() as i64),
        };
        assert!(
            store
                .append_activity(first.identity(), &[late])
                .await?
                .fenced
        );
        assert_eq!(
            store.activity(&attempts[0].assignment_id, 0, 64).await?,
            retained
        );
        assert_eq!(
            store.capture_state(&attempts[0].assignment_id).await?,
            state
        );
        assert_eq!(
            store
                .attempt_by_assignment(&id, &attempts[0].assignment_id)
                .await?
                .assignment_state,
            AssignmentState::Running,
            "a stop decision does not invent execution death"
        );
        for peer in &attempts[1..] {
            fake.replace(&peer.operation_id, WorkState::Completed(empty_result(peer)));
            controller.drive_operation(&peer.operation_id).await?;
            let current = store
                .attempt_by_assignment(&id, &peer.assignment_id)
                .await?;
            assert_eq!(current.assignment_state, AssignmentState::Succeeded);
            assert_eq!(current.revision, attempts[0].revision);
        }
        let read = controller
            .read_attempt(AuditAttemptReadParams {
                run_id: id.clone(),
                assignment_id: attempts[0].assignment_id.clone(),
                after_sequence: None,
                limit: None,
            })
            .await?;
        assert!(!read.attempt.completed && read.raw_result.is_none());
        assert_eq!(fake.starts.load(Ordering::SeqCst), 3);
        fake.interrupt_release.notify_one();
    }
    Ok(())
}

#[tokio::test]
async fn retry_acknowledgement_loss_replays_exactly_one_fresh_identity_and_rejects_stale_controls()
-> anyhow::Result<()> {
    let (_temp, controller, fake, id, attempts) = setup().await?;
    let original = capture(&controller, &fake, &attempts[0]).await?;
    let held = original.hold_flush().await;
    let history = "HISTORY_ONLY_PARTIAL";
    delta(&original, history).await;
    let request = control(&id, &attempts[0].assignment_id, "retry-once", "retry");
    let mut unauthorised = request.clone();
    unauthorised["authoriseRetry"] = json!(false);
    assert!(
        api(&controller, "audit/attempt/control", unauthorised)
            .await
            .is_err()
    );
    let mut unacknowledged = request.clone();
    unacknowledged["acknowledgePossibleOverlap"] = json!(false);
    assert!(
        api(&controller, "audit/attempt/control", unacknowledged)
            .await
            .is_err()
    );
    controller
        .fail_control_response
        .store(true, Ordering::SeqCst);
    assert!(
        api(&controller, "audit/attempt/control", request.clone())
            .await
            .is_err(),
        "lost acknowledgement follows the commit"
    );
    controller
        .fail_control_response
        .store(false, Ordering::SeqCst);
    let after = controller.store().await?.attempts(&id).await?;
    assert_eq!(after.len(), 4);
    let replacement = after
        .iter()
        .find(|attempt| attempt.reviewer_id == attempts[0].reviewer_id && attempt.current)
        .expect("replacement");
    assert_ne!(replacement.assignment_id, attempts[0].assignment_id);
    assert_ne!(replacement.operation_id, attempts[0].operation_id);
    assert_eq!(replacement.attempt_ordinal, 2);
    assert_eq!(replacement.snapshot_id, attempts[0].snapshot_id);
    assert_eq!(replacement.reviewer_ordinal, attempts[0].reviewer_ordinal);
    assert!(
        controller
            .store()
            .await?
            .execution(&replacement.operation_id)
            .await?
            .is_none()
    );
    let replay = api(&controller, "audit/attempt/control", request.clone())
        .await
        .expect("replay");
    assert_eq!(replay["replacementAssignmentId"], replacement.assignment_id);
    let other = AuditController::new(
        controller.config.clone(),
        controller.models.clone(),
        fake.clone(),
        Arc::new(|_| {}),
    );
    assert_eq!(
        api(&other, "audit/attempt/control", request.clone())
            .await
            .expect("cross-controller replay"),
        replay
    );
    let mut conflict = request.clone();
    conflict["reason"] = json!("different authority");
    assert!(
        api(&controller, "audit/attempt/control", conflict)
            .await
            .is_err()
    );
    assert!(
        api(
            &controller,
            "audit/attempt/control",
            control(&id, &attempts[0].assignment_id, "stale", "cancel")
        )
        .await
        .is_err()
    );
    let store = controller.store().await?;
    assert_eq!(store.attempts(&id).await?.len(), 4);
    assert_eq!(
        store.activity(&attempts[0].assignment_id, 0, 64).await?[0].payload,
        history
    );
    assert!(
        store
            .activity(&replacement.assignment_id, 0, 64)
            .await?
            .is_empty()
    );
    assert!(
        !store
            .attempt_input(&replacement.operation_id)
            .await?
            .0
            .contains(history)
    );
    assert_eq!(
        store.attempt_input(&replacement.operation_id).await?,
        store.attempt_input(&attempts[0].operation_id).await?
    );
    drop(held);
    delta(&original, "STALE_COLLECTOR").await;
    assert_eq!(
        store
            .activity(&attempts[0].assignment_id, 0, 64)
            .await?
            .len(),
        1
    );
    let reopened = AuditStore::open(controller.root.clone()).await?;
    assert_eq!(reopened.attempts(&id).await?, store.attempts(&id).await?);
    controller.tick().await?;
    assert_eq!(fake.starts.load(Ordering::SeqCst), 4);
    assert_eq!(
        store
            .attempt_by_assignment(&id, &attempts[1].assignment_id)
            .await?
            .attempt_revision,
        attempts[1].attempt_revision
    );
    Ok(())
}

#[tokio::test]
async fn completed_result_wins_stale_cancel_and_journal_text_never_establishes_completion()
-> anyhow::Result<()> {
    let (_temp, controller, fake, id, attempts) = setup().await?;
    let capture = capture(&controller, &fake, &attempts[0]).await?;
    let full = String::from_utf8(empty_result(&attempts[0]))?;
    delta(&capture, &full).await;
    capture.flush().await?;
    assert_eq!(
        controller
            .store()
            .await?
            .attempt_by_assignment(&id, &attempts[0].assignment_id)
            .await?
            .assignment_state,
        AssignmentState::Running
    );
    fake.replace(
        &attempts[0].operation_id,
        WorkState::Completed(full.as_bytes().to_vec()),
    );
    controller
        .drive_operation(&attempts[0].operation_id)
        .await?;
    let before = controller
        .store()
        .await?
        .attempt_by_assignment(&id, &attempts[0].assignment_id)
        .await?;
    let outcome = api(
        &controller,
        "audit/attempt/control",
        control(
            &id,
            &before.assignment_id,
            "stale-completed-cancel",
            "cancel",
        ),
    )
    .await
    .expect("completed wins");
    assert_eq!(outcome["completedWon"], true);
    assert_eq!(
        controller
            .store()
            .await?
            .attempt_by_assignment(&id, &before.assignment_id)
            .await?,
        before
    );
    assert!(!before.capacity_released);
    Ok(())
}

#[tokio::test]
async fn competing_controller_freezes_committed_prefix_when_collector_is_unavailable()
-> anyhow::Result<()> {
    let (_temp, controller, fake, id, attempts) = setup().await?;
    let original = capture(&controller, &fake, &attempts[0]).await?;
    delta(&original, "committed").await;
    original.flush().await?;
    let held = original.hold_flush().await;
    delta(&original, "UNCOMMITTED_TAIL").await;
    let peer_fake = Arc::new(Fake::default());
    *peer_fake.root.lock().expect("root") = Some(controller.root.clone());
    let peer = AuditController::new(
        controller.config.clone(),
        controller.models.clone(),
        peer_fake,
        Arc::new(|_| {}),
    );
    let decision = tokio::time::timeout(
        Duration::from_secs(2),
        api(
            &peer,
            "audit/attempt/control",
            control(&id, &attempts[0].assignment_id, "competing-stop", "cancel"),
        ),
    )
    .await?
    .expect("competing decision");
    assert_eq!(decision["cutoff"], 1);
    let store = controller.store().await?;
    let prefix = store.activity(&attempts[0].assignment_id, 0, 64).await?;
    assert_eq!(prefix[0].payload, "committed");
    let state = store.capture_state(&attempts[0].assignment_id).await?;
    assert!(
        state
            .gap
            .as_deref()
            .is_some_and(|gap| gap.contains("uncommitted"))
    );
    drop(held);
    original.flush().await?;
    assert_eq!(
        store.activity(&attempts[0].assignment_id, 0, 64).await?,
        prefix
    );
    assert_eq!(
        store.capture_state(&attempts[0].assignment_id).await?,
        state
    );
    Ok(())
}

#[tokio::test]
async fn capture_deduplicates_identified_replay_and_complete_overlap_but_preserves_repeated_prose()
-> anyhow::Result<()> {
    use codex_protocol::items::{AgentMessageContent, AgentMessageItem, TurnItem};
    use codex_protocol::protocol::{ItemCompletedEvent, ReasoningContentDeltaEvent};
    let (_temp, controller, fake, id, attempts) = setup().await?;
    let capture = capture(&controller, &fake, &attempts[0]).await?;
    let held = capture.hold_flush().await;
    delta(&capture, "echo ").await;
    delta(&capture, "echo ").await;
    capture
        .observe(&Event {
            id: "turn".to_owned(),
            msg: EventMsg::ReasoningContentDelta(ReasoningContentDeltaEvent {
                thread_id: capture.identity().session_id.clone(),
                turn_id: "turn".to_owned(),
                item_id: "private".to_owned(),
                delta: "PRIVATE_REASONING".to_owned(),
                summary_index: 0,
            }),
        })
        .await;
    drop(held);
    capture.flush().await?;
    let store = controller.store().await?;
    let replay = ActivityInput {
        producer_key: "delta:turn:message:0".to_owned(),
        turn_id: "turn".to_owned(),
        item_id: "message".to_owned(),
        kind: "assistant_delta".to_owned(),
        payload: "echo ".to_owned(),
        text_offset: Some(0),
    };
    assert_eq!(
        store
            .append_activity(capture.identity(), &[replay.clone()])
            .await?
            .sequence,
        2
    );
    let mut stale = capture.identity().clone();
    stale.owner_generation += 1;
    assert!(
        store
            .append_activity(&stale, &[replay.clone()])
            .await?
            .fenced
    );
    capture
        .observe(&Event {
            id: "turn".to_owned(),
            msg: EventMsg::ItemCompleted(ItemCompletedEvent {
                thread_id: codex_protocol::ThreadId::from_string(&capture.identity().session_id)?,
                turn_id: "turn".to_owned(),
                item: TurnItem::AgentMessage(AgentMessageItem {
                    id: "message".to_owned(),
                    content: vec![AgentMessageContent::Text {
                        text: "echo echo ".to_owned(),
                    }],
                    phase: None,
                    memory_citation: None,
                    delivery: None,
                    questions: None,
                }),
                started_at_ms: None,
                completed_at_ms: 0,
            }),
        })
        .await;
    capture.flush().await?;
    let entries = store.activity(&attempts[0].assignment_id, 0, 64).await?;
    assert_eq!(entries.len(), 3);
    assert_eq!(
        entries
            .iter()
            .map(|entry| entry.payload.as_str())
            .collect::<String>(),
        "echo echo "
    );
    assert_eq!(entries[2].kind, "assistant_message");
    assert!(
        entries[2].payload.is_empty(),
        "complete-message overlap is metadata, not duplicated text"
    );
    let mut conflicting = replay;
    conflicting.payload = "different".to_owned();
    assert!(
        store
            .append_activity(capture.identity(), &[conflicting])
            .await
            .is_err()
    );
    assert_eq!(
        store
            .capture_state(&attempts[0].assignment_id)
            .await?
            .sequence,
        3
    );
    assert_eq!(
        store
            .attempt_by_assignment(&id, &attempts[0].assignment_id)
            .await?
            .attempt_revision,
        attempts[0].attempt_revision
    );
    assert_eq!(
        store
            .attempt_by_assignment(&id, &attempts[0].assignment_id)
            .await?
            .revision,
        attempts[0].revision
    );
    let reopened = AuditStore::open(controller.root.clone()).await?;
    assert_eq!(
        reopened.activity(&attempts[0].assignment_id, 0, 64).await?,
        entries
    );
    assert_eq!(
        store
            .attempt_by_assignment(&id, &attempts[0].assignment_id)
            .await?
            .assignment_state,
        AssignmentState::Running
    );
    Ok(())
}

#[tokio::test]
async fn interval_and_size_flush_commit_without_advancing_attempt_or_run_revisions()
-> anyhow::Result<()> {
    let (_temp, controller, fake, id, attempts) = setup().await?;
    let capture = capture(&controller, &fake, &attempts[0]).await?;
    let store = controller.store().await?;
    // Prepare the DB on real time; only advance the collector's small interval.
    tokio::time::pause();
    delta(&capture, "interval partial").await;
    tokio::time::advance(crate::audex::capture::FLUSH_INTERVAL).await;
    tokio::time::resume();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if store
                .capture_state(&attempts[0].assignment_id)
                .await?
                .sequence
                == 1
            {
                break anyhow::Ok(());
            }
            tokio::task::yield_now().await;
        }
    })
    .await??;
    delta(&capture, &"x".repeat(64 * 1024)).await;
    assert_eq!(
        store
            .capture_state(&attempts[0].assignment_id)
            .await?
            .sequence,
        2,
        "size flush finishes before returning"
    );
    let state = store
        .attempt_by_assignment(&id, &attempts[0].assignment_id)
        .await?;
    assert_eq!(state.attempt_revision, attempts[0].attempt_revision);
    assert_eq!(state.revision, attempts[0].revision);
    Ok(())
}

#[tokio::test]
async fn recovery_records_a_possible_uncommitted_gap_without_replaying_or_inventing_output()
-> anyhow::Result<()> {
    let (_temp, controller, fake, id, attempts) = setup().await?;
    let capture = capture(&controller, &fake, &attempts[0]).await?;
    let held = capture.hold_flush().await;
    delta(&capture, "NOT_DURABLE").await;
    let store = controller.store().await?;
    sqlx::query("UPDATE operations SET lease_expires_at_ms=0 WHERE id=?")
        .bind(&attempts[0].operation_id)
        .execute(&store.pool_for_tests())
        .await?;
    let missing = Arc::new(Fake::default());
    *missing.root.lock().expect("root") = Some(controller.root.clone());
    let recovered = AuditController::new(
        controller.config.clone(),
        controller.models.clone(),
        missing.clone(),
        Arc::new(|_| {}),
    );
    recovered.drive_operation(&attempts[0].operation_id).await?;
    let state = store.capture_state(&attempts[0].assignment_id).await?;
    assert!(
        state
            .gap
            .as_deref()
            .is_some_and(|gap| gap.contains("may be missing"))
    );
    assert_eq!(state.sequence, 0);
    assert!(
        store
            .activity(&attempts[0].assignment_id, 0, 64)
            .await?
            .is_empty()
    );
    assert_eq!(
        store
            .attempt_by_assignment(&id, &attempts[0].assignment_id)
            .await?
            .assignment_state,
        AssignmentState::Running
    );
    assert_eq!(missing.starts.load(Ordering::SeqCst), 0);
    assert_eq!(fake.starts.load(Ordering::SeqCst), 3);
    drop(held);
    Ok(())
}

#[tokio::test]
async fn activity_retrieval_detects_missing_entries_and_corrupt_attribution() -> anyhow::Result<()>
{
    let (_temp, controller, fake, _id, attempts) = setup().await?;
    let capture = capture(&controller, &fake, &attempts[0]).await?;
    delta(&capture, "one").await;
    delta(&capture, "two").await;
    capture.flush().await?;
    let store = controller.store().await?;
    sqlx::query(
        "UPDATE audit_activity SET kind='tool_result' WHERE assignment_id=? AND sequence=1",
    )
    .bind(&attempts[0].assignment_id)
    .execute(&store.pool_for_tests())
    .await?;
    assert!(
        store
            .activity(&attempts[0].assignment_id, 0, 64)
            .await
            .is_err()
    );
    sqlx::query(
        "UPDATE audit_activity SET kind='assistant_delta' WHERE assignment_id=? AND sequence=1",
    )
    .bind(&attempts[0].assignment_id)
    .execute(&store.pool_for_tests())
    .await?;
    sqlx::query("DELETE FROM audit_activity WHERE assignment_id=? AND sequence=2")
        .bind(&attempts[0].assignment_id)
        .execute(&store.pool_for_tests())
        .await?;
    assert!(
        store
            .activity(&attempts[0].assignment_id, 0, 64)
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn capture_failure_and_truncation_preserve_history_and_cannot_become_success()
-> anyhow::Result<()> {
    for truncate in [false, true] {
        let (_temp, controller, fake, id, attempts) = setup().await?;
        let capture = capture(&controller, &fake, &attempts[0]).await?;
        delta(&capture, "retained prefix").await;
        capture.flush().await?;
        let store = controller.store().await?;
        if truncate {
            sqlx::query("UPDATE audit_attempts SET output_bytes=? WHERE assignment_id=?")
                .bind(crate::audex::store::activity::MAX_CAPTURE_BYTES - 1)
                .bind(&attempts[0].assignment_id)
                .execute(&store.pool_for_tests())
                .await?;
        } else {
            sqlx::query("CREATE TRIGGER fixture_capture_write_failure BEFORE INSERT ON audit_activity BEGIN SELECT RAISE(ABORT,'fixture disk failure'); END")
                .execute(&store.pool_for_tests()).await?;
        }
        delta(&capture, "unretainable tail").await;
        let flushed = capture.flush().await;
        assert_eq!(flushed.is_err(), !truncate);
        let state = store.capture_state(&attempts[0].assignment_id).await?;
        assert!(state.failure.is_some() && state.gap.is_some());
        assert_eq!(state.truncated, truncate);
        assert_eq!(state.sequence, 1);
        let text_bytes: i64 =
            sqlx::query_scalar("SELECT text_bytes FROM audit_activity_items WHERE assignment_id=?")
                .bind(&attempts[0].assignment_id)
                .fetch_one(&store.pool_for_tests())
                .await?;
        assert_eq!(
            text_bytes,
            "retained prefix".len() as i64,
            "discarded text cannot advance the durable item prefix"
        );
        // Positive terminal history cannot override a failed capture contract.
        fake.replace(
            &attempts[0].operation_id,
            WorkState::Completed(empty_result(&attempts[0])),
        );
        controller
            .drive_operation(&attempts[0].operation_id)
            .await?;
        let failed = store
            .attempt_by_assignment(&id, &attempts[0].assignment_id)
            .await?;
        assert_eq!(failed.assignment_state, AssignmentState::Failed);
        assert_eq!(
            store.capture_state(&failed.assignment_id).await?.cutoff,
            Some(1)
        );
        assert_eq!(
            store.activity(&failed.assignment_id, 0, 64).await?[0].payload,
            "retained prefix"
        );
        assert!(store.raw_result(&failed)?.is_none());
        assert!(store.append_activity(capture.identity(), &[]).await?.fenced);
        fake.replace(
            &attempts[1].operation_id,
            WorkState::Completed(empty_result(&attempts[1])),
        );
        controller
            .drive_operation(&attempts[1].operation_id)
            .await?;
        assert_eq!(
            store
                .attempt_by_assignment(&id, &attempts[1].assignment_id)
                .await?
                .assignment_state,
            AssignmentState::Succeeded
        );
    }
    Ok(())
}

#[tokio::test]
async fn turn_end_during_uncommitted_control_does_not_claim_the_buffer_is_durable()
-> anyhow::Result<()> {
    use codex_protocol::protocol::TurnCompleteEvent;
    let (_temp, controller, fake, _id, attempts) = setup().await?;
    let capture = capture(&controller, &fake, &attempts[0]).await?;
    delta(&capture, "buffered before control").await;
    let prefix = capture.control_prefix();
    assert_eq!(prefix.len(), 1);
    capture
        .observe(&Event {
            id: "turn".to_owned(),
            msg: EventMsg::TurnComplete(TurnCompleteEvent {
                root_turn_id: None,
                turn_id: "turn".to_owned(),
                last_agent_message: None,
                error: None,
                started_at: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            }),
        })
        .await;
    let store = controller.store().await?;
    let before = store.capture_state(&attempts[0].assignment_id).await?;
    assert!(!before.finished);
    assert_eq!(before.sequence, 0);
    // A rejected control releases its marker; the ordinary timer/flush still
    // retains the complete available prefix and then marks capture finished.
    capture.control_finished(None);
    capture.flush().await?;
    let after = store.capture_state(&attempts[0].assignment_id).await?;
    assert!(after.finished);
    assert_eq!(after.sequence, 2);
    assert_eq!(
        store.activity(&attempts[0].assignment_id, 0, 64).await?[0].payload,
        "buffered before control"
    );
    assert_eq!(
        store.attempts(&attempts[0].run_id).await?[0].assignment_state,
        AssignmentState::Running
    );
    Ok(())
}

#[tokio::test]
async fn invalid_or_failed_terminal_evidence_keeps_the_available_prefix_without_success()
-> anyhow::Result<()> {
    for invalid_result in [true, false] {
        let (_temp, controller, fake, id, attempts) = setup().await?;
        let capture = capture(&controller, &fake, &attempts[0]).await?;
        let held = capture.hold_flush().await;
        delta(&capture, "{unfinished JSON before terminal evidence").await;
        fake.replace(
            &attempts[0].operation_id,
            if invalid_result {
                WorkState::Completed(b"{invalid terminal JSON".to_vec())
            } else {
                WorkState::Failed("fixture failure".to_owned())
            },
        );
        controller
            .drive_operation(&attempts[0].operation_id)
            .await?;
        let store = controller.store().await?;
        let failed = store
            .attempt_by_assignment(&id, &attempts[0].assignment_id)
            .await?;
        assert_eq!(failed.assignment_state, AssignmentState::Failed);
        assert_eq!(
            store.capture_state(&failed.assignment_id).await?.cutoff,
            Some(1)
        );
        assert_eq!(
            store.activity(&failed.assignment_id, 0, 64).await?[0].payload,
            "{unfinished JSON before terminal evidence"
        );
        assert_eq!(store.raw_result(&failed)?.is_some(), invalid_result);
        drop(held);
    }
    Ok(())
}

#[tokio::test]
async fn three_collectors_commit_independently_while_one_attempt_is_cancelled() -> anyhow::Result<()>
{
    let (_temp, controller, fake, id, attempts) = setup().await?;
    let first = capture(&controller, &fake, &attempts[0]).await?;
    let second = capture(&controller, &fake, &attempts[1]).await?;
    let third = capture(&controller, &fake, &attempts[2]).await?;
    let held = first.hold_flush().await;
    delta(&first, "first prefix").await;
    delta(&second, "second prefix").await;
    delta(&third, "third prefix").await;
    let (decision, two, three) = tokio::join!(
        api(
            &controller,
            "audit/attempt/control",
            control(
                &id,
                &attempts[0].assignment_id,
                "independent-control",
                "cancel"
            )
        ),
        second.flush(),
        third.flush(),
    );
    decision.expect("decision");
    two?;
    three?;
    let store = controller.store().await?;
    for (attempt, text) in attempts
        .iter()
        .zip(["first prefix", "second prefix", "third prefix"])
    {
        assert_eq!(
            store.activity(&attempt.assignment_id, 0, 64).await?[0].payload,
            text
        );
        assert_eq!(
            store
                .attempt_by_assignment(&id, &attempt.assignment_id)
                .await?
                .revision,
            attempt.revision
        );
    }
    for peer in &attempts[1..] {
        assert_eq!(
            store
                .attempt_by_assignment(&id, &peer.assignment_id)
                .await?
                .attempt_revision,
            peer.attempt_revision
        );
    }
    fake.replace(
        &attempts[0].operation_id,
        WorkState::Completed(empty_result(&attempts[0])),
    );
    controller
        .drive_operation(&attempts[0].operation_id)
        .await?;
    assert!(
        store
            .raw_result(
                &store
                    .attempt_by_assignment(&id, &attempts[0].assignment_id)
                    .await?
            )?
            .is_none()
    );
    drop(held);
    Ok(())
}

#[tokio::test]
async fn unsubmitted_retry_needs_no_overlap_consent_and_replacement_alone_supplies_the_report()
-> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let (controller, fake, _) = fixture(&temp).await;
    let id = start(&controller, "queued-retry").await;
    let store = controller.store().await?;
    let original = store.load_run_metadata(&id).await?;
    let mut request = control(&id, &original.assignment_id, "replace-queued", "retry");
    request["acknowledgePossibleOverlap"] = json!(false);
    let decision = api(&controller, "audit/attempt/control", request)
        .await
        .expect("unsubmitted retry");
    assert_eq!(decision["possibleOverlap"], false);
    controller.tick().await?;
    assert_eq!(fake.starts.load(Ordering::SeqCst), 1);
    let second = store.load_run_metadata(&id).await?;
    assert_ne!(second.assignment_id, original.assignment_id);
    let capture = capture(&controller, &fake, &second).await?;
    delta(&capture, "SUPERSEDED_TEXT_MUST_NOT_ENTER_REPORT").await;
    let third = api(
        &controller,
        "audit/attempt/control",
        control(&id, &second.assignment_id, "replace-live", "retry"),
    )
    .await
    .expect("live retry");
    controller.tick().await?;
    let replacement = store.load_run_metadata(&id).await?;
    assert_eq!(replacement.assignment_id, third["replacementAssignmentId"]);
    assert_eq!(replacement.attempt_ordinal, 3);
    fake.replace(
        &replacement.operation_id,
        WorkState::Completed(empty_result(&replacement)),
    );
    controller.tick().await?;
    controller.tick().await?;
    let finished = read(&controller, &id).await;
    assert_eq!(finished["phase"], "completed");
    let report = finished["report"].as_str().context("report")?;
    assert!(report.contains("No findings"));
    assert!(!report.contains("SUPERSEDED_TEXT_MUST_NOT_ENTER_REPORT"));
    assert!(
        store.activity(&second.assignment_id, 0, 64).await?[0]
            .payload
            .contains("SUPERSEDED_TEXT")
    );
    assert_eq!(fake.starts.load(Ordering::SeqCst), 2);
    Ok(())
}

#[tokio::test]
async fn legacy_capacity_release_after_retry_targets_the_current_attempt() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let (controller, fake, _) = fixture(&temp).await;
    let id = start(&controller, "retry-compatibility").await;
    controller.tick().await?;
    let store = controller.store().await?;
    let original = store.load_run_metadata(&id).await?;
    api(
        &controller,
        "audit/attempt/control",
        control(&id, &original.assignment_id, "replacement", "retry"),
    )
    .await
    .expect("retry");
    controller.tick().await?;
    let replacement = store.load_run_metadata(&id).await?;
    assert!(!replacement.capacity_released);
    store
        .record_observation_issue(
            &replacement.operation_id,
            replacement.attempt_revision,
            "outcome_unconfirmed",
        )
        .await?;
    let revision = store.request_cancel(&id, replacement.revision).await?;
    store
        .release_capacity(&id, revision, "fixture explicit release")
        .await?;
    assert!(store.load_run_metadata(&id).await?.capacity_released);
    assert_eq!(fake.starts.load(Ordering::SeqCst), 2);
    Ok(())
}

#[tokio::test]
async fn retry_validates_retained_input_before_retiring_the_original_attempt() -> anyhow::Result<()>
{
    let (_temp, controller, _fake, id, attempts) = setup().await?;
    let store = controller.store().await?;
    sqlx::query("UPDATE audit_attempt_inputs SET request_json='{}' WHERE assignment_id=?")
        .bind(&attempts[0].assignment_id)
        .execute(&store.pool_for_tests())
        .await?;
    assert!(
        api(
            &controller,
            "audit/attempt/control",
            control(
                &id,
                &attempts[0].assignment_id,
                "invalid-retained-input",
                "retry"
            )
        )
        .await
        .is_err()
    );
    assert_eq!(store.attempts(&id).await?.len(), 3);
    assert_eq!(
        store
            .attempt_by_assignment(&id, &attempts[0].assignment_id)
            .await?,
        attempts[0]
    );
    assert_eq!(
        store
            .capture_state(&attempts[0].assignment_id)
            .await?
            .disposition,
        "active"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM audit_attempt_actions")
            .fetch_one(&store.pool_for_tests())
            .await?,
        0
    );
    Ok(())
}

#[tokio::test]
async fn oversized_event_identity_fails_capture_without_retaining_unbounded_metadata()
-> anyhow::Result<()> {
    let (_temp, controller, fake, _id, attempts) = setup().await?;
    let capture = capture(&controller, &fake, &attempts[0]).await?;
    capture
        .observe(&Event {
            id: "turn".to_owned(),
            msg: EventMsg::AgentMessageContentDelta(AgentMessageContentDeltaEvent {
                thread_id: capture.identity().session_id.clone(),
                turn_id: "turn".to_owned(),
                item_id: "x".repeat(513),
                delta: "public fragment".to_owned(),
            }),
        })
        .await;
    let state = controller
        .store()
        .await?
        .capture_state(&attempts[0].assignment_id)
        .await?;
    assert!(state.failure.is_some());
    assert_eq!(state.sequence, 0);
    assert!(
        controller
            .store()
            .await?
            .activity(&attempts[0].assignment_id, 0, 64)
            .await?
            .is_empty()
    );
    Ok(())
}
