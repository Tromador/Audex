//! S1 contract fixtures: private rosters, in-process fake execution and isolated stores only.
mod opinions;
mod source_tools;
use super::*;
use crate::audex::store::attempts::{AttemptAction, ControlRequest};
use crate::audex::store::{ReportPublishOutcome, ResultCommitOutcome};
use anyhow::Context;
use std::time::Duration;

fn control(
    run: &str,
    attempt: &AuditRunRecord,
    key: &str,
    action: AttemptAction,
) -> ControlRequest {
    ControlRequest {
        run_id: run.to_owned(),
        assignment_id: attempt.assignment_id.clone(),
        action_key: key.to_owned(),
        action,
        reason: "fixture operator decision".to_owned(),
        authorise_retry: true,
        acknowledge_possible_overlap: true,
    }
}

async fn complete(
    controller: &AuditController,
    attempt: &AuditRunRecord,
    finding: bool,
) -> anyhow::Result<()> {
    let store = controller.store().await?;
    let snapshot = controller
        .snapshots()
        .open(&SnapshotId::parse(attempt.snapshot_id.clone())?)
        .await?;
    let current = store
        .attempt_by_assignment(&attempt.run_id, &attempt.assignment_id)
        .await?;
    attest_fake_completion(store, &current).await?;
    let mut value: Value = serde_json::from_slice(&empty_result(attempt))?;
    if finding {
        value["summary"] = json!("An independently attributed defect.");
        value["findings"] = json!([{"severity":"high","confidence":"medium","title":"Attributed finding",
            "mechanism":"Fixture mechanism","impact":"Fixture impact","remediation":null,
            "evidence":[{"description":"Retained source","path":"example.rs","start_byte":0,"end_byte":2,"start_line":1,"end_line":1}]}]);
    }
    assert!(matches!(
        store
            .commit_seeded_result(
                &attempt.operation_id,
                current.attempt_revision,
                &snapshot,
                &serde_json::to_vec(&value)?
            )
            .await?,
        ResultCommitOutcome::Committed { .. }
    ));
    Ok(())
}

fn record(outcome: ReportPublishOutcome) -> crate::audex::store::ReportRecord {
    match outcome {
        ReportPublishOutcome::Published { report, .. }
        | ReportPublishOutcome::AlreadyPublished { report, .. } => report,
    }
}

async fn counts(store: &AuditStore, run: &str) -> anyhow::Result<(i64, i64, i64)> {
    Ok(sqlx::query_as("SELECT (SELECT count(*) FROM reviewer_assignments WHERE run_id=?),
        (SELECT count(*) FROM operations o JOIN reviewer_assignments a ON a.id=o.assignment_id WHERE a.run_id=?),
        (SELECT count(*) FROM audit_attempt_actions x JOIN audit_attempts t ON t.assignment_id=x.assignment_id WHERE t.run_id=?)")
        .bind(run).bind(run).bind(run).fetch_one(&store.pool_for_tests()).await?)
}

#[tokio::test]
async fn three_reviewers_findings_zero_findings_and_exact_partial_render_in_order()
-> anyhow::Result<()> {
    let (_temp, controller, fake, id, attempts) = super::attempts::setup().await?;
    let store = controller.store().await?;
    complete(&controller, &attempts[0], true).await?;
    complete(&controller, &attempts[1], false).await?;
    let capture = super::attempts::capture(&controller, &fake, &attempts[2]).await?;
    let held = capture.hold_flush().await;
    let fragment = "{\"unfinished\":\"only received prefix";
    super::attempts::delta(&capture, fragment).await;
    capture
        .tool(
            "turn",
            "read",
            "tool_result",
            "SOURCE_TOOL_ACTIVITY_ONLY".to_owned(),
        )
        .await?;
    let decision = controller.control_attempt(serde_json::from_value(json!({"runId":id,"assignmentId":attempts[2].assignment_id,
        "actionKey":"last-cancel","action":"cancel","reason":"fixture stop","authoriseRetry":false,"acknowledgePossibleOverlap":false}))?).await?;
    assert_eq!(decision.cutoff, Some(2));
    assert_eq!(
        store.load_run_metadata(&id).await?.phase,
        RunPhase::Reporting
    );
    let before = counts(&store, &id).await?;
    assert!(
        store
            .control_attempt(
                &control(&id, &attempts[2], "too-late", AttemptAction::Retry),
                None
            )
            .await
            .is_err()
    );
    assert_eq!(counts(&store, &id).await?, before);
    let input = store.freeze_report(&id).await?;
    assert_eq!(input.outcome, "success");
    assert_eq!(input.reviewers.len(), 3);
    for (index, reviewer) in input.reviewers.iter().enumerate() {
        assert_eq!(reviewer.reviewer_id, attempts[index].reviewer_id);
        assert_eq!(reviewer.assignment_id, attempts[index].assignment_id);
        assert_eq!(reviewer.reviewer_ordinal, index as i64);
        assert_eq!(reviewer.observation_verified, index < 2);
        assert_eq!(
            reviewer.observed_model.as_deref(),
            if index < 2 {
                Some(attempts[index].model.as_str())
            } else {
                None
            }
        );
    }
    assert_eq!(
        input.reviewers[0]
            .completed
            .as_ref()
            .context("completed")?
            .findings
            .len(),
        1
    );
    assert!(
        input.reviewers[1]
            .completed
            .as_ref()
            .context("zero findings completed")?
            .findings
            .is_empty()
    );
    assert_eq!(input.reviewers[2].activity[0].payload, fragment);
    let report = record(store.publish_frozen(&input).await?);
    let bytes = store.read_report(&report)?;
    let text = String::from_utf8(bytes.clone())?;
    assert!(text.find("## Reviewer 1").unwrap() < text.find("## Reviewer 2").unwrap());
    assert!(text.find("## Reviewer 2").unwrap() < text.find("## Reviewer 3").unwrap());
    assert!(text.contains("Attributed finding") && text.contains("No findings were reported."));
    assert!(text.contains(fragment) && text.contains("unvalidated; not findings"));
    assert!(text.contains("fixture stop"));
    drop(held);
    super::attempts::delta(&capture, "LATE_COMPLETION").await;
    assert_eq!(store.read_report(&report)?, bytes);
    assert_eq!(store.load_run(&id).await?.phase, RunPhase::Completed);
    assert_eq!(fake.starts.load(Ordering::SeqCst), 3);
    Ok(())
}

#[tokio::test]
async fn last_cancel_or_ignore_closes_retry_before_freeze_and_survives_reopen() -> anyhow::Result<()>
{
    for action in [AttemptAction::Cancel, AttemptAction::Ignore] {
        let temp = tempfile::tempdir()?;
        let (controller, fake, _) = fixture(&temp).await;
        let id = start(&controller, "single-stop").await;
        let store = controller.store().await?;
        let attempt = store.load_run_metadata(&id).await?;
        store
            .control_attempt(&control(&id, &attempt, "last-stop", action), None)
            .await?;
        let row: (bool, Option<String>) = sqlx::query_as(
            "SELECT collection_ready,authoritative_selection_id FROM audit_runs WHERE id=?",
        )
        .bind(&id)
        .fetch_one(&store.pool_for_tests())
        .await?;
        assert_eq!(row, (true, None));
        let reopened = AuditStore::open(controller.root.clone()).await?;
        let before = counts(&reopened, &id).await?;
        assert!(
            reopened
                .control_attempt(
                    &control(&id, &attempt, "late-retry", AttemptAction::Retry),
                    None
                )
                .await
                .is_err()
        );
        assert_eq!(counts(&reopened, &id).await?, before);
        controller.tick().await?; // Reporting is driven even though reviewer zero is excluded.
        let view = read(&controller, &id).await;
        assert_eq!(view["phase"], "cancelled");
        assert_eq!(view["outcome"], "no_completed_review");
        assert!(
            view["report"]
                .as_str()
                .context("cancelled report")?
                .contains("incomplete / excluded")
        );
        assert_eq!(fake.starts.load(Ordering::SeqCst), 0);
    }
    Ok(())
}

#[tokio::test]
async fn retry_and_final_completion_order_atomically_in_both_orders() -> anyhow::Result<()> {
    for retry_first in [true, false] {
        let (_temp, controller, fake, id, attempts) = super::attempts::setup().await?;
        let store = controller.store().await?;
        complete(&controller, &attempts[0], false).await?;
        store
            .control_attempt(
                &control(&id, &attempts[1], "cancel-b", AttemptAction::Cancel),
                None,
            )
            .await?;
        let (entered, release) =
            store.pause_transition_for_test(if retry_first { "retry" } else { "result" });
        let first_controller = controller.clone();
        let first_store = store.clone();
        let peer = attempts[2].clone();
        let request = control(&id, &attempts[1], "retry-b", AttemptAction::Retry);
        let first_request = request.clone();
        let first = tokio::spawn(async move {
            if retry_first {
                first_store
                    .control_attempt(&first_request, None)
                    .await
                    .map(|_| ())
                    .map_err(anyhow::Error::from)
            } else {
                complete(&first_controller, &peer, false).await
            }
        });
        tokio::time::timeout(Duration::from_secs(3), entered.notified()).await?;
        let second_controller = controller.clone();
        let second_store = store.clone();
        let peer = attempts[2].clone();
        let second = tokio::spawn(async move {
            if retry_first {
                complete(&second_controller, &peer, false).await
            } else {
                second_store
                    .control_attempt(&request, None)
                    .await
                    .map(|_| ())
                    .map_err(anyhow::Error::from)
            }
        });
        tokio::task::yield_now().await;
        release.add_permits(1);
        first.await??;
        let second_result = second.await?;
        assert_eq!(second_result.is_ok(), retry_first);
        let reopened = AuditStore::open(controller.root.clone()).await?;
        let (ready, selection): (bool, Option<String>) = sqlx::query_as(
            "SELECT collection_ready,authoritative_selection_id FROM audit_runs WHERE id=?",
        )
        .bind(&id)
        .fetch_one(&reopened.pool_for_tests())
        .await?;
        assert_eq!(ready, !retry_first);
        assert!(selection.is_none(), "readiness precedes freeze");
        assert_eq!(
            reopened.attempts(&id).await?.len(),
            if retry_first { 4 } else { 3 }
        );
        assert_eq!(
            fake.starts.load(Ordering::SeqCst),
            3,
            "Retry alone queues; no implicit inference"
        );
        if !retry_first {
            let before = counts(&reopened, &id).await?;
            assert!(
                reopened
                    .control_attempt(
                        &control(
                            &id,
                            &attempts[1],
                            "stale-confirmation",
                            AttemptAction::Retry
                        ),
                        None
                    )
                    .await
                    .is_err()
            );
            assert_eq!(counts(&reopened, &id).await?, before);
        }
    }
    Ok(())
}

#[tokio::test]
async fn ignore_is_permanent_and_active_failed_unknown_support_direct_retry() -> anyhow::Result<()>
{
    for state in [
        AssignmentState::Running,
        AssignmentState::Failed,
        AssignmentState::Unknown,
    ] {
        let (_temp, controller, _fake, id, attempts) = super::attempts::setup().await?;
        let store = controller.store().await?;
        store
            .control_attempt(
                &control(&id, &attempts[0], "ignore", AttemptAction::Ignore),
                None,
            )
            .await?;
        let before = counts(&store, &id).await?;
        for (key, action) in [
            ("retry-ignored", AttemptAction::Retry),
            ("cancel-ignored", AttemptAction::Cancel),
        ] {
            assert!(
                store
                    .control_attempt(&control(&id, &attempts[0], key, action), None)
                    .await
                    .is_err()
            );
        }
        assert_eq!(counts(&store, &id).await?, before);
        if state != AssignmentState::Running {
            store
                .finish_operation(
                    &attempts[1].operation_id,
                    attempts[1].attempt_revision,
                    state,
                    "fixture failure / uncertainty",
                )
                .await?;
        }
        assert_eq!(
            store.load_run_metadata(&id).await?.phase,
            RunPhase::Reviewing
        );
        let retried = store
            .control_attempt(
                &control(&id, &attempts[1], "direct-retry", AttemptAction::Retry),
                None,
            )
            .await?;
        assert!(retried.replacement_assignment_id.is_some());
        let replay = store
            .control_attempt(
                &control(&id, &attempts[1], "direct-retry", AttemptAction::Retry),
                None,
            )
            .await?;
        assert_eq!(retried, replay);
        assert_eq!(counts(&store, &id).await?.0, 4);
        assert!(
            store
                .control_attempt(
                    &control(&id, &attempts[0], "still-ignored", AttemptAction::Retry),
                    None
                )
                .await
                .is_err()
        );
    }
    Ok(())
}

#[tokio::test]
async fn abort_retains_completed_and_buffered_partials_without_waiting_for_interrupt()
-> anyhow::Result<()> {
    let (_temp, controller, fake, id, attempts) = super::attempts::setup().await?;
    let store = controller.store().await?;
    complete(&controller, &attempts[0], false).await?;
    let capture = super::attempts::capture(&controller, &fake, &attempts[1]).await?;
    let held = capture.hold_flush().await;
    super::attempts::delta(&capture, "PRE_ABORT_UNFINISHED_JSON {").await;
    store
        .finish_operation(
            &attempts[2].operation_id,
            attempts[2].attempt_revision,
            AssignmentState::Unknown,
            "fixture unknown",
        )
        .await?;
    fake.pause_interrupt.store(true, Ordering::SeqCst);
    let response = tokio::time::timeout(
        Duration::from_secs(3),
        api(&controller, "audit/cancel", json!({"runId":id})),
    )
    .await?
    .expect("durable Abort response");
    assert_eq!(response["run"]["phase"], "reporting");
    assert!(
        response["run"]["report"].is_null(),
        "acknowledgement precedes publication"
    );
    tokio::time::timeout(Duration::from_secs(3), fake.interrupt_entered.notified()).await?;
    let input = store.freeze_report(&id).await?;
    assert_eq!(input.outcome, "aborted_by_operator");
    assert!(input.reviewers[0].completed.is_some());
    assert_eq!(
        input.reviewers[1].activity[0].payload,
        "PRE_ABORT_UNFINISHED_JSON {"
    );
    assert_eq!(input.reviewers[2].assignment_state, "unknown");
    let before = counts(&store, &id).await?;
    assert!(
        store
            .control_attempt(
                &control(&id, &attempts[1], "post-abort-retry", AttemptAction::Retry),
                None
            )
            .await
            .is_err()
    );
    assert!(complete(&controller, &attempts[1], false).await.is_err());
    assert_eq!(counts(&store, &id).await?, before);
    drop(held);
    super::attempts::delta(&capture, "POST_ABORT_MUST_NOT_ENTER").await;
    let report = record(store.publish_frozen(&input).await?);
    let bytes = store.read_report(&report)?;
    assert!(!String::from_utf8_lossy(&bytes).contains("POST_ABORT"));
    let reopened = AuditStore::open(controller.root.clone()).await?;
    // Lost acknowledgement and post-publication replays select nothing new.
    reopened.abort_run(&id, &[]).await?;
    assert_eq!(
        reopened.freeze_report(&id).await?.selection_id,
        input.selection_id
    );
    assert_eq!(record(reopened.publish_report(&id, 0).await?), report);
    assert_eq!(
        read(&controller, &id).await["outcome"],
        "aborted_by_operator"
    );
    fake.interrupt_release.notify_waiters();
    assert_eq!(fake.starts.load(Ordering::SeqCst), 3);
    Ok(())
}

#[tokio::test]
async fn abort_fences_queued_peers_and_unavailable_capture_has_honest_gap() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let (controller, fake, _) = fixture(&temp).await; // One worker; two queued peers.
    let id = super::foundations::roster(&controller, "abort-queued", 3).await?;
    controller.tick().await?;
    let store = controller.store().await?;
    let attempts = store.attempts(&id).await?;
    let _capture = super::attempts::capture(&controller, &fake, &attempts[0]).await?;
    store.abort_run(&id, &[]).await?; // Competing owner has no local capture buffer.
    let input = store.freeze_report(&id).await?;
    assert!(
        input.reviewers[0]
            .capture_gap
            .as_ref()
            .context("unavailable gap")?
            .contains("collector unavailable")
    );
    controller.tick().await?;
    assert_eq!(fake.starts.load(Ordering::SeqCst), 1);
    assert_eq!(
        store.load_run(&id).await?.outcome.as_deref(),
        Some("aborted_by_operator")
    );
    assert!(
        store
            .attempts(&id)
            .await?
            .iter()
            .all(|a| a.capacity_released)
    );
    Ok(())
}

#[tokio::test]
async fn abort_supersedes_normal_freeze_and_candidate_then_recovers_only_authoritative_selection()
-> anyhow::Result<()> {
    for candidate_written in [false, true] {
        let (_temp, controller, fake, id, attempts) = super::attempts::setup().await?;
        let store = controller.store().await?;
        for attempt in &attempts {
            complete(&controller, attempt, false).await?;
        }
        let normal = store.freeze_report(&id).await?;
        let rendered = crate::audex::report::collection::render(&normal)?;
        if candidate_written {
            let path = format!("reports/{}/{}.md", id, rendered.input_digest);
            crate::audex::store::publish_immutable(
                controller.root.as_path(),
                &path,
                &rendered.bytes,
                &rendered.content_digest,
            )?;
        }
        api(&controller, "audit/cancel", json!({"runId":id}))
            .await
            .expect("Abort while normal report pending");
        let aborted = store.freeze_report(&id).await?;
        assert_ne!(normal.selection_id, aborted.selection_id);
        assert_eq!(aborted.generation, normal.generation + 1);
        assert!(store.publish_frozen(&normal).await.is_err());
        let reopened = AuditStore::open(controller.root.clone()).await?;
        let expected = crate::audex::report::collection::render(&aborted)?;
        let report = record(reopened.publish_report(&id, 0).await?);
        assert_eq!(report.id, expected.id);
        assert_eq!(report.content_digest, expected.content_digest);
        assert_eq!(reopened.read_report(&report)?, expected.bytes);
        reopened.abort_run(&id, &[]).await?;
        assert_eq!(record(reopened.publish_report(&id, 0).await?), report);
        let n: i64 =
            sqlx::query_scalar("SELECT count(*) FROM audit_report_selections WHERE run_id=?")
                .bind(&id)
                .fetch_one(&store.pool_for_tests())
                .await?;
        assert_eq!(n, 2);
        assert_eq!(fake.starts.load(Ordering::SeqCst), 3);
    }
    Ok(())
}

#[tokio::test]
async fn competing_abort_and_terminal_publication_commit_one_outcome_in_both_orders()
-> anyhow::Result<()> {
    for abort_first in [true, false] {
        let (_temp, controller, _fake, id, attempts) = super::attempts::setup().await?;
        let store = controller.store().await?;
        for attempt in &attempts {
            complete(&controller, attempt, false).await?;
        }
        store.freeze_report(&id).await?;
        let (entered, release) =
            store.pause_transition_for_test(if abort_first { "abort" } else { "publish" });
        let first_store = store.clone();
        let first_id = id.clone();
        let first = tokio::spawn(async move {
            if abort_first {
                first_store.abort_run(&first_id, &[]).await.map(|_| ())
            } else {
                first_store.publish_report(&first_id, 0).await.map(|_| ())
            }
        });
        tokio::time::timeout(Duration::from_secs(3), entered.notified()).await?;
        let second_store = store.clone();
        let second_id = id.clone();
        let second = tokio::spawn(async move {
            if abort_first {
                second_store.publish_report(&second_id, 0).await.map(|_| ())
            } else {
                second_store.abort_run(&second_id, &[]).await.map(|_| ())
            }
        });
        tokio::task::yield_now().await;
        release.add_permits(1);
        first.await??;
        // A renderer which started with the old selection may be fenced; recovery
        // always resumes the authoritative selection instead of resurrecting it.
        let _ = second.await?;
        store.publish_report(&id, 0).await?;
        let terminal = store.load_run(&id).await?;
        assert_eq!(
            terminal.outcome.as_deref(),
            Some(if abort_first {
                "aborted_by_operator"
            } else {
                "success"
            })
        );
        let report = terminal.report.context("one published report")?;
        store.abort_run(&id, &[]).await?;
        assert_eq!(store.load_run(&id).await?.report, Some(report));
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM reports WHERE run_id=?")
            .bind(&id)
            .fetch_one(&store.pool_for_tests())
            .await?;
        assert_eq!(n, 1);
    }
    Ok(())
}

#[tokio::test]
async fn freeze_file_commit_and_acknowledgement_windows_recover_identical_bytes_without_inference()
-> anyhow::Result<()> {
    let (_temp, controller, fake, id, attempts) = super::attempts::setup().await?;
    let store = controller.store().await?;
    for attempt in &attempts {
        complete(&controller, attempt, false).await?;
    }
    let input = store.freeze_report(&id).await?;
    let expected = crate::audex::report::collection::render(&input)?;
    let reopened = AuditStore::open(controller.root.clone()).await?;
    assert_eq!(
        crate::audex::report::collection::render(&reopened.freeze_report(&id).await?)?.bytes,
        expected.bytes
    );
    let path = format!("reports/{}/{}.md", id, expected.input_digest);
    crate::audex::store::publish_immutable(
        controller.root.as_path(),
        &path,
        &expected.bytes,
        &expected.content_digest,
    )?;
    let cold = AuditStore::open(controller.root.clone()).await?;
    let (a, b) = tokio::join!(cold.publish_report(&id, 0), reopened.publish_report(&id, 0));
    let report = record(a?);
    assert_eq!(record(b?), report);
    let after_ack_loss = AuditStore::open(controller.root.clone()).await?;
    assert_eq!(record(after_ack_loss.publish_report(&id, 0).await?), report);
    assert_eq!(after_ack_loss.read_report(&report)?, expected.bytes);
    controller.tick().await?;
    assert_eq!(fake.starts.load(Ordering::SeqCst), 3);
    Ok(())
}

#[tokio::test]
async fn selected_integrity_is_required_but_superseded_artefacts_are_not_dependencies()
-> anyhow::Result<()> {
    let (_temp, controller, _fake, id, attempts) = super::attempts::setup().await?;
    let store = controller.store().await?;
    complete(&controller, &attempts[0], false).await?;
    // Rejected output on a predecessor is retained history, never a dependency of its replacement.
    let snapshot = controller
        .snapshots()
        .open(&SnapshotId::parse(attempts[1].snapshot_id.clone())?)
        .await?;
    assert!(matches!(
        store
            .commit_seeded_result(
                &attempts[1].operation_id,
                attempts[1].attempt_revision,
                &snapshot,
                b"   "
            )
            .await?,
        ResultCommitOutcome::Rejected { .. }
    ));
    store
        .control_attempt(
            &control(&id, &attempts[1], "retry", AttemptAction::Retry),
            None,
        )
        .await?;
    let historical = store
        .attempt_by_assignment(&id, &attempts[1].assignment_id)
        .await?;
    assert!(!historical.current);
    let historical_path = historical
        .result_path
        .as_ref()
        .context("historical rejected artefact")?;
    std::fs::remove_file(controller.root.join(historical_path).as_path())?;
    store.abort_run(&id, &[]).await?;
    let input = store.freeze_report(&id).await?;
    assert!(
        !input
            .reviewers
            .iter()
            .any(|r| r.assignment_id == historical.assignment_id)
    );
    assert!(store.publish_frozen(&input).await.is_ok());
    let completed = input.reviewers[0]
        .result_path
        .as_ref()
        .context("selected result path")?;
    make_writable(controller.root.join(completed).as_path())?;
    std::fs::write(controller.root.join(completed).as_path(), b"tampered")?;
    assert!(store.freeze_report(&id).await.is_err());
    Ok(())
}

#[tokio::test]
async fn completion_and_cancel_or_ignore_preserve_the_winning_contribution() -> anyhow::Result<()> {
    for action in [AttemptAction::Cancel, AttemptAction::Ignore] {
        for complete_first in [true, false] {
            let (_temp, controller, _fake, id, attempts) = super::attempts::setup().await?;
            let store = controller.store().await?;
            if complete_first {
                complete(&controller, &attempts[0], false).await?;
            }
            let decision = store
                .control_attempt(&control(&id, &attempts[0], "stop", action.clone()), None)
                .await?;
            assert_eq!(decision.completed_won, complete_first);
            if !complete_first {
                assert!(complete(&controller, &attempts[0], false).await.is_err());
            }
            for peer in &attempts[1..] {
                complete(&controller, peer, false).await?;
            }
            let input = store.freeze_report(&id).await?;
            assert_eq!(input.reviewers[0].completed.is_some(), complete_first);
            assert_eq!(input.outcome, "success");
            assert!(
                store
                    .control_attempt(
                        &control(&id, &attempts[0], "late-retry", AttemptAction::Retry),
                        None
                    )
                    .await
                    .is_err()
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn abort_and_last_completion_order_preserves_only_pre_abort_acceptance() -> anyhow::Result<()>
{
    for result_first in [true, false] {
        let (_temp, controller, _fake, id, attempts) = super::attempts::setup().await?;
        let store = controller.store().await?;
        complete(&controller, &attempts[0], false).await?;
        complete(&controller, &attempts[1], false).await?;
        let (entered, release) =
            store.pause_transition_for_test(if result_first { "result" } else { "abort" });
        let first_store = store.clone();
        let first_controller = controller.clone();
        let first_id = id.clone();
        let peer = attempts[2].clone();
        let first = tokio::spawn(async move {
            if result_first {
                complete(&first_controller, &peer, false).await
            } else {
                first_store
                    .abort_run(&first_id, &[])
                    .await
                    .map(|_| ())
                    .map_err(anyhow::Error::from)
            }
        });
        tokio::time::timeout(Duration::from_secs(3), entered.notified()).await?;
        let second_store = store.clone();
        let second_controller = controller.clone();
        let second_id = id.clone();
        let peer = attempts[2].clone();
        let second = tokio::spawn(async move {
            if result_first {
                second_store
                    .abort_run(&second_id, &[])
                    .await
                    .map(|_| ())
                    .map_err(anyhow::Error::from)
            } else {
                complete(&second_controller, &peer, false).await
            }
        });
        tokio::task::yield_now().await;
        release.add_permits(1);
        first.await??;
        assert_eq!(second.await?.is_ok(), result_first);
        let input = store.freeze_report(&id).await?;
        assert_eq!(input.outcome, "aborted_by_operator");
        assert_eq!(input.reviewers[2].completed.is_some(), result_first);
        assert!(input.reviewers[..2].iter().all(|r| r.completed.is_some()));
        store.publish_frozen(&input).await?;
    }
    Ok(())
}

#[tokio::test]
async fn durable_abort_does_not_depend_on_report_integrity_or_publication_success()
-> anyhow::Result<()> {
    let (_temp, controller, _fake, id, attempts) = super::attempts::setup().await?;
    let store = controller.store().await?;
    complete(&controller, &attempts[0], false).await?;
    let accepted = store
        .attempt_by_assignment(&id, &attempts[0].assignment_id)
        .await?;
    let path = controller
        .root
        .join(accepted.result_path.as_ref().context("accepted result")?);
    let original = std::fs::read(path.as_path())?;
    make_writable(path.as_path())?;
    std::fs::write(path.as_path(), b"corrupted selected result")?;
    let ack = api(&controller, "audit/cancel", json!({"runId":id}))
        .await
        .expect("durable stop acknowledgement does not open corrupt report inputs");
    assert_eq!(ack["run"]["phase"], "reporting");
    assert!(ack["run"]["report"].is_null());
    assert_eq!(
        store.load_run_metadata(&id).await?.control_intent,
        "cancel_requested"
    );
    assert!(store.publish_report(&id, 0).await.is_err());
    assert!(store.load_run_metadata(&id).await?.report.is_none());
    // Restoring the exact retained bytes recovers the same frozen selection.
    std::fs::write(path.as_path(), original)?;
    store.publish_report(&id, 0).await?;
    assert_eq!(
        store.load_run(&id).await?.outcome.as_deref(),
        Some("aborted_by_operator")
    );
    Ok(())
}

#[tokio::test]
async fn selected_partial_integrity_errors_are_explicit_and_do_not_reopen_collection()
-> anyhow::Result<()> {
    let (_temp, controller, fake, id, attempts) = super::attempts::setup().await?;
    let store = controller.store().await?;
    let capture = super::attempts::capture(&controller, &fake, &attempts[0]).await?;
    let held = capture.hold_flush().await;
    super::attempts::delta(&capture, "selected partial").await;
    api(&controller, "audit/cancel", json!({"runId":id}))
        .await
        .expect("Abort");
    let input = store.freeze_report(&id).await?;
    drop(held);
    sqlx::query("DELETE FROM audit_activity WHERE assignment_id=? AND sequence=1")
        .bind(&attempts[0].assignment_id)
        .execute(&store.pool_for_tests())
        .await?;
    assert!(store.publish_report(&id, 0).await.is_err());
    assert!(store.publish_frozen(&input).await.is_err());
    assert!(store.load_run_metadata(&id).await?.report.is_none());
    assert!(
        store
            .control_attempt(
                &control(&id, &attempts[0], "late-retry", AttemptAction::Retry),
                None
            )
            .await
            .is_err()
    );
    Ok(())
}

#[cfg(unix)]
fn make_writable(path: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path)?.permissions().mode();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode | 0o200))
}
#[cfg(not(unix))]
#[allow(clippy::permissions_set_readonly_false)]
fn make_writable(path: &std::path::Path) -> std::io::Result<()> {
    let mut permissions = std::fs::metadata(path)?.permissions();
    permissions.set_readonly(false);
    std::fs::set_permissions(path, permissions)
}

#[tokio::test]
async fn partial_only_collection_preserves_unfinished_text_without_claiming_a_completed_opinion()
-> anyhow::Result<()> {
    let (_temp, controller, fake, id, attempts) = super::attempts::setup().await?;
    let store = controller.store().await?;
    let capture = super::attempts::capture(&controller, &fake, &attempts[0]).await?;
    let held = capture.hold_flush().await;
    let fragment = "{\"findings\":[UNFINISHED_PARTIAL_ONLY";
    super::attempts::delta(&capture, fragment).await;
    capture
        .tool(
            "turn",
            "read",
            "tool_result",
            "TOOL_ONLY_NOT_A_FINDING".to_owned(),
        )
        .await?;
    let prefix = capture.control_prefix();
    let decision = store
        .control_attempt(
            &control(&id, &attempts[0], "partial-cancel", AttemptAction::Cancel),
            Some((capture.identity(), &prefix)),
        )
        .await?;
    capture.control_finished(Some(&decision));
    for (index, peer) in attempts[1..].iter().enumerate() {
        store
            .control_attempt(
                &control(&id, peer, &format!("ignore-{index}"), AttemptAction::Ignore),
                None,
            )
            .await?;
    }
    let input = store.freeze_report(&id).await?;
    assert_eq!(input.outcome, "no_completed_review");
    assert!(
        input
            .reviewers
            .iter()
            .all(|reviewer| reviewer.completed.is_none())
    );
    assert_eq!(input.reviewers[0].activity[0].payload, fragment);
    assert_eq!(input.reviewers[1].disposition, "ignored");
    let report = record(store.publish_frozen(&input).await?);
    let bytes = store.read_report(&report)?;
    let text = String::from_utf8(bytes.clone())?;
    assert!(text.contains(fragment) && text.contains("TOOL_ONLY_NOT_A_FINDING"));
    assert!(!text.contains("No findings were reported.") && !text.contains("### Findings"));
    drop(held);
    super::attempts::delta(&capture, "]}").await;
    assert!(complete(&controller, &attempts[0], false).await.is_err());
    sqlx::query("UPDATE reviewer_assignments SET usage_json=? WHERE id=?")
        .bind(r#"{"late":true}"#)
        .bind(&attempts[0].assignment_id)
        .execute(&store.pool_for_tests())
        .await?;
    assert_eq!(store.read_report(&report)?, bytes);
    assert_eq!(
        store.load_run(&id).await?.outcome.as_deref(),
        Some("no_completed_review")
    );
    Ok(())
}

#[path = "report_format4.rs"]
mod format4;
