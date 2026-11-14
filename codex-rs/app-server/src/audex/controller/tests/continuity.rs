//! Whole executions are recovered by evidence; generations and source calls do not count.
use super::*;

#[tokio::test]
async fn one_automatic_replacement_preserves_source_route_and_allows_later_operator_attempts()
-> anyhow::Result<()> {
    let (_temp, controller, fake, id, attempts) = attempts::setup().await?;
    let original = &attempts[0];
    let _capture = attempts::capture(&controller, &fake, original).await?;
    let store = controller.store().await?;
    fake.replace(
        &original.operation_id,
        WorkState::Failed("transient execution loss".into()),
    );
    store.mark_execution_stopped(&original.operation_id).await?;
    controller.drive_operation(&original.operation_id).await?;
    controller.drive_operation(&original.operation_id).await?;
    let records = store.attempts(&id).await?;
    let next = records
        .iter()
        .find(|a| a.current && a.reviewer_id == original.reviewer_id)
        .unwrap();
    assert_eq!(records.len(), 4);
    assert_eq!(next.attempt_ordinal, 2);
    assert_eq!(next.snapshot_id, original.snapshot_id);
    assert_eq!(next.provider, original.provider);
    assert_eq!(next.model, original.model);
    assert_eq!(
        store.attempt_input(&next.operation_id).await?,
        store.attempt_input(&original.operation_id).await?
    );
    controller.drive_operation(&next.operation_id).await?;
    let next = store.run_for_operation(&next.operation_id).await?;
    let _capture = attempts::capture(&controller, &fake, &next).await?;
    fake.replace(
        &next.operation_id,
        WorkState::Failed("replacement failed".into()),
    );
    store.mark_execution_stopped(&next.operation_id).await?;
    controller.drive_operation(&next.operation_id).await?;
    for _ in 0..3 {
        controller.drive_operation(&next.operation_id).await?;
    }
    assert_eq!(store.attempts(&id).await?.len(), 4);
    assert_eq!(
        store.recovery_status(&next.assignment_id).await?.as_deref(),
        Some("automatic_recovery_exhausted_operator_investigation_required")
    );
    let retried = api(&controller, "audit/attempt/control", json!({
        "runId":id,"assignmentId":next.assignment_id,"action":"retry","actionKey":"operator-third",
        "reason":"investigate after recovery failure","authoriseRetry":true,"acknowledgePossibleOverlap":false
    })).await.expect("explicit third attempt after automatic recovery stops");
    assert!(retried["replacementAssignmentId"].is_string());
    assert_eq!(store.attempts(&id).await?.len(), 5);
    assert_eq!(store.list_run_ids("", 100).await?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn any_surviving_review_text_or_uncertain_liveness_blocks_automatic_replacement()
-> anyhow::Result<()> {
    for (text, stopped) in [
        ("x", true),
        ("Looks complete but lacks confirmation", true),
        ("", false),
    ] {
        let (_temp, controller, fake, id, attempts) = attempts::setup().await?;
        let original = &attempts[0];
        let capture = attempts::capture(&controller, &fake, original).await?;
        if !text.is_empty() {
            attempts::delta(&capture, text).await;
        }
        let store = controller.store().await?;
        fake.replace(
            &original.operation_id,
            WorkState::Failed("lost execution".into()),
        );
        if stopped {
            store.mark_execution_stopped(&original.operation_id).await?;
        }
        controller.drive_operation(&original.operation_id).await?;
        controller.drive_operation(&original.operation_id).await?;
        assert_eq!(store.attempts(&id).await?.len(), 3);
        assert_eq!(
            store
                .recovery_status(&original.assignment_id)
                .await?
                .as_deref(),
            Some(if text.is_empty() {
                "original_execution_stop_unconfirmed"
            } else {
                "operator_retry_required_surviving_output"
            })
        );
        if !text.is_empty() {
            assert!(
                store
                    .activity(&original.assignment_id, 0, 64)
                    .await?
                    .iter()
                    .any(|entry| entry.payload == text)
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn recovered_terminal_and_recovered_unconfirmed_history_are_not_rerun() -> anyhow::Result<()>
{
    for completed in [false, true] {
        let (_temp, controller, fake, id, attempts) = attempts::setup().await?;
        let original = &attempts[0];
        let _capture = attempts::capture(&controller, &fake, original).await?;
        let store = controller.store().await?;
        store.mark_execution_stopped(&original.operation_id).await?;
        if completed {
            fake.replace(
                &original.operation_id,
                WorkState::Failed("initial observation was incomplete".into()),
            );
            controller.drive_operation(&original.operation_id).await?;
            fake.replace(
                &original.operation_id,
                WorkState::Completed(empty_result(original)),
            );
        } else {
            store
                .retain_recovered_output(&original.operation_id, "Recovered unfinished sentence")
                .await?;
            fake.replace(
                &original.operation_id,
                WorkState::Failed("no terminal confirmation".into()),
            );
        }
        controller.drive_operation(&original.operation_id).await?;
        controller.drive_operation(&original.operation_id).await?;
        assert_eq!(store.attempts(&id).await?.len(), 3);
        let read = controller
            .read_attempt(AuditAttemptReadParams {
                run_id: id,
                assignment_id: original.assignment_id.clone(),
                after_sequence: None,
                limit: None,
            })
            .await?;
        assert_eq!(read.attempt.completed, completed);
        if !completed {
            assert_eq!(
                read.recovered_output.as_deref(),
                Some("Recovered unfinished sentence")
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn recovered_output_is_exposed_only_for_failed_or_uncertain_executions() -> anyhow::Result<()>
{
    for state in [
        AssignmentState::Running,
        AssignmentState::Succeeded,
        AssignmentState::Failed,
        AssignmentState::Unknown,
    ] {
        let (_temp, controller, fake, id, attempts) = attempts::setup().await?;
        let original = &attempts[0];
        let capture = attempts::capture(&controller, &fake, original).await?;
        attempts::delta(&capture, "Durable interim output").await;
        capture.flush().await?;
        let store = controller.store().await?;
        // Even a field populated by an older observer must not mislabel healthy work.
        store
            .retain_recovered_output(&original.operation_id, "Original history snapshot")
            .await?;
        match state {
            AssignmentState::Succeeded => {
                fake.replace(
                    &original.operation_id,
                    WorkState::Completed(empty_result(original)),
                );
                controller.drive_operation(&original.operation_id).await?;
            }
            AssignmentState::Failed | AssignmentState::Unknown => {
                let current = store.run_for_operation(&original.operation_id).await?;
                controller
                    .finish_observed_operation(
                        &original.operation_id,
                        current.attempt_revision,
                        state,
                        "lost original execution",
                    )
                    .await?;
            }
            _ => {}
        }
        let read = controller
            .read_attempt(AuditAttemptReadParams {
                run_id: id,
                assignment_id: original.assignment_id.clone(),
                after_sequence: None,
                limit: None,
            })
            .await?;
        assert_eq!(
            read.recovered_output.as_deref(),
            matches!(state, AssignmentState::Failed | AssignmentState::Unknown)
                .then_some("Original history snapshot")
        );
        assert!(
            read.entries
                .iter()
                .any(|entry| entry.payload == "Durable interim output")
        );
        assert_eq!(
            store
                .recovered_output(&original.operation_id)
                .await?
                .as_deref(),
            Some("Original history snapshot")
        );
    }
    Ok(())
}

#[tokio::test]
async fn recovered_fragments_span_flushes_pages_and_completed_messages_without_duplication()
-> anyhow::Result<()> {
    use crate::audex::store::activity::ActivityInput;
    for state in [AssignmentState::Failed, AssignmentState::Unknown] {
        let (_temp, controller, fake, id, attempts) = attempts::setup().await?;
        let original = &attempts[0];
        let capture = attempts::capture(&controller, &fake, original).await?;
        let store = controller.store().await?;
        // More than a read page; every fragment is flushed out of the capture tail.
        for _ in 0..260 {
            attempts::delta(&capture, "é").await;
            capture.flush().await?;
        }
        let complete = format!("{} complete", "é".repeat(260));
        store
            .append_activity(
                capture.identity(),
                &[ActivityInput {
                    producer_key: "complete-first-item".into(),
                    turn_id: "turn".into(),
                    item_id: "message".into(),
                    kind: "assistant_message".into(),
                    payload: complete.clone(),
                    text_offset: None,
                }],
            )
            .await?;
        let next = ActivityInput {
            producer_key: "next-item".into(),
            turn_id: "turn".into(),
            item_id: "second".into(),
            kind: "assistant_delta".into(),
            payload: "Next ".into(),
            text_offset: Some(0),
        };
        store
            .append_activity(capture.identity(), &[next.clone()])
            .await?;
        store.append_activity(capture.identity(), &[next]).await?; // idempotent replay
        let held = capture.hold_flush().await;
        capture
            .offer(ActivityInput {
                producer_key: "next-tail".into(),
                turn_id: "turn".into(),
                item_id: "second".into(),
                kind: "assistant_delta".into(),
                payload: "unfinished sentence".into(),
                text_offset: Some(5),
            })
            .await?;
        if state == AssignmentState::Failed {
            capture
                .offer(ActivityInput {
                    producer_key: "next-complete".into(),
                    turn_id: "turn".into(),
                    item_id: "second".into(),
                    kind: "assistant_message".into(),
                    payload: "Next unfinished sentence".into(),
                    text_offset: None,
                })
                .await?;
        }
        if state == AssignmentState::Unknown {
            fake.work.lock().expect("work").insert(
                original.operation_id.clone(),
                Observation::Uncertain("original execution identity became uncertain".into()),
            );
        } else {
            fake.replace(
                &original.operation_id,
                WorkState::Failed("lost execution".into()),
            );
        }
        controller.drive_operation(&original.operation_id).await?;
        assert_eq!(
            store
                .run_for_operation(&original.operation_id)
                .await?
                .assignment_state,
            state
        );
        drop(held);
        let expected = format!("{complete}\n\nNext unfinished sentence");
        for _ in 0..2 {
            let read = controller
                .read_attempt(AuditAttemptReadParams {
                    run_id: id.clone(),
                    assignment_id: original.assignment_id.clone(),
                    after_sequence: None,
                    limit: None,
                })
                .await?;
            assert_eq!(read.recovered_output.as_deref(), Some(expected.as_str()));
        }
        let reopened = AuditStore::open(controller.root.clone()).await?;
        assert_eq!(
            reopened
                .recovered_output(&original.operation_id)
                .await?
                .as_deref(),
            Some(expected.as_str())
        );
        assert!(!reopened.recovery_ready(&original.assignment_id).await?);
    }
    Ok(())
}
