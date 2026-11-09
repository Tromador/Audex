//! Completed unstructured opinions obey the same transaction/control ordering.
use super::*;

async fn opinion(controller: &AuditController, attempt: &AuditRunRecord) -> anyhow::Result<()> {
    let store = controller.store().await?;
    let current = store.run_for_operation(&attempt.operation_id).await?;
    attest_fake_completion(store, &current).await?;
    let snapshot = controller
        .snapshots()
        .open(&SnapshotId::parse(current.snapshot_id.clone())?)
        .await?;
    store
        .commit_seeded_result(
            &current.operation_id,
            current.attempt_revision,
            &snapshot,
            b"An independent completed opinion with no structured extraction.",
        )
        .await?;
    Ok(())
}

#[tokio::test]
async fn unstructured_opinion_and_exact_controls_linearise_in_both_orders() -> anyhow::Result<()> {
    for action in [
        AttemptAction::Cancel,
        AttemptAction::Ignore,
        AttemptAction::Retry,
    ] {
        for completion_first in [true, false] {
            let (_temp, controller, fake, id, attempts) = super::super::attempts::setup().await?;
            let store = controller.store().await?;
            // A separate wanted peer keeps collection open while this exact target races.
            complete(&controller, &attempts[0], false).await?;
            let target = attempts[1].clone();
            let request = control(&id, &target, "opinion-control", action.clone());
            let (entered, release) = store.pause_transition_for_test(if completion_first {
                "result"
            } else {
                match action {
                    AttemptAction::Cancel => "cancel",
                    AttemptAction::Ignore => "ignore",
                    AttemptAction::Retry => "retry",
                }
            });
            let c = controller.clone();
            let t = target.clone();
            let q = request.clone();
            let first = tokio::spawn(async move {
                if completion_first {
                    opinion(&c, &t).await
                } else {
                    c.store()
                        .await?
                        .control_attempt(&q, None)
                        .await
                        .map(|_| ())
                        .map_err(Into::into)
                }
            });
            tokio::time::timeout(Duration::from_secs(3), entered.notified()).await?;
            let c = controller.clone();
            let t = target.clone();
            let second = tokio::spawn(async move {
                if completion_first {
                    c.store()
                        .await?
                        .control_attempt(&request, None)
                        .await
                        .map(|_| ())
                        .map_err(anyhow::Error::from)
                } else {
                    opinion(&c, &t).await
                }
            });
            tokio::task::yield_now().await;
            release.add_permits(1);
            first.await??;
            let _ = second.await?; // A stale Retry/control/result may be refused; never retargeted.
            let retained = store
                .attempt_by_assignment(&id, &target.assignment_id)
                .await?;
            assert_eq!(
                retained.assignment_state == AssignmentState::Succeeded,
                completion_first
            );
            assert_eq!(
                store
                    .result_extraction(&target.assignment_id)
                    .await?
                    .is_some(),
                completion_first
            );
            assert_eq!(
                fake.starts.load(Ordering::SeqCst),
                3,
                "control creates no implicit inference"
            );
            store.abort_run(&id, &[]).await?;
            let frozen = store.freeze_report(&id).await?;
            assert!(frozen.reviewers[0].completed.is_some());
            assert_eq!(frozen.reviewers[1].opinion.is_some(), completion_first);
            assert!(frozen.reviewers[1].completed.is_none());
            let bytes = crate::audex::report::collection::render(&frozen)?.bytes;
            let reopened = AuditStore::open(controller.root.clone()).await?;
            let report = record(reopened.publish_report(&id, 0).await?);
            assert_eq!(reopened.read_report(&report)?, bytes);
            assert_eq!(record(store.publish_report(&id, 0).await?), report);
        }
    }
    Ok(())
}

#[tokio::test]
async fn unstructured_completion_and_abort_freeze_only_the_winner_without_replay()
-> anyhow::Result<()> {
    for completion_first in [true, false] {
        let (_temp, controller, fake, id, attempts) = super::super::attempts::setup().await?;
        let store = controller.store().await?;
        complete(&controller, &attempts[0], false).await?;
        complete(&controller, &attempts[1], false).await?;
        let target = attempts[2].clone();
        let (entered, release) =
            store.pause_transition_for_test(if completion_first { "result" } else { "abort" });
        let c = controller.clone();
        let t = target.clone();
        let run = id.clone();
        let first = tokio::spawn(async move {
            if completion_first {
                opinion(&c, &t).await
            } else {
                c.store()
                    .await?
                    .abort_run(&run, &[])
                    .await
                    .map(|_| ())
                    .map_err(Into::into)
            }
        });
        tokio::time::timeout(Duration::from_secs(3), entered.notified()).await?;
        let c = controller.clone();
        let t = target.clone();
        let run = id.clone();
        let second = tokio::spawn(async move {
            if completion_first {
                c.store()
                    .await?
                    .abort_run(&run, &[])
                    .await
                    .map(|_| ())
                    .map_err(anyhow::Error::from)
            } else {
                opinion(&c, &t).await
            }
        });
        tokio::task::yield_now().await;
        release.add_permits(1);
        first.await??;
        assert_eq!(second.await?.is_ok(), completion_first);
        let frozen = store.freeze_report(&id).await?;
        assert_eq!(frozen.outcome, "aborted_by_operator");
        assert_eq!(frozen.reviewers[2].opinion.is_some(), completion_first);
        assert!(frozen.reviewers[2].completed.is_none());
        let before = serde_json::to_vec(&frozen)?;
        store.publish_frozen(&frozen).await?;
        controller.tick().await?;
        assert_eq!(
            serde_json::to_vec(&store.freeze_report(&id).await?)?,
            before
        );
        assert_eq!(fake.starts.load(Ordering::SeqCst), 3);
    }
    Ok(())
}

#[tokio::test]
async fn completion_policy_retains_only_completed_opinions_and_identifies_stopped_jobs()
-> anyhow::Result<()> {
    for aborted in [false, true] {
        for completed in 0..=2 {
            for unstructured in [false, true] {
                if completed == 0 && unstructured {
                    continue;
                }
                let (_temp, controller, fake, id, attempts) =
                    super::super::attempts::setup().await?;
                let store = controller.store().await?;
                for (index, attempt) in attempts.iter().take(completed).enumerate() {
                    if index == 0 && unstructured {
                        opinion(&controller, attempt).await?;
                    } else {
                        complete(&controller, attempt, index == 0).await?;
                    }
                }
                store
                    .finish_operation(
                        &attempts[2].operation_id,
                        attempts[2].attempt_revision,
                        AssignmentState::Failed,
                        "fixture_failure",
                    )
                    .await?;
                if aborted {
                    store.abort_run(&id, &[]).await?;
                } else {
                    for attempt in &attempts[completed..] {
                        store
                            .control_attempt(
                                &control(
                                    &id,
                                    attempt,
                                    &format!("cancel-{}", attempt.assignment_id),
                                    AttemptAction::Cancel,
                                ),
                                None,
                            )
                            .await?;
                    }
                }
                let frozen = store.freeze_report(&id).await?;
                let diagnostic = crate::audex::report::collection::render(&frozen)?.bytes;
                let report = record(store.publish_frozen(&frozen).await?);
                assert_eq!(store.read_report(&report)?, diagnostic);
                let view = controller.view(&id).await?;
                assert_eq!(view.collection.completed_count, completed as u32);
                assert_eq!(view.report.as_deref().unwrap().as_bytes(), diagnostic);
                let summary = view.findings_summary.as_ref().unwrap();
                assert!(summary.contains("Reviewer 3:"));
                assert!(summary.contains("execution state at publication: failed"));
                assert!(summary.contains("aborted/cancelled by operator"));
                assert!(summary.contains(r"fixture\_failure"));
                assert_eq!(summary.contains("Audit aborted by operator"), aborted);
                match completed {
                    0 => assert!(summary.contains("No reviewer completed")),
                    1 => assert!(summary.contains("One reviewer completed")),
                    _ => assert!(summary.contains("2 reviewers completed")),
                }
                assert_eq!(
                    summary.contains("Relationships between claims"),
                    completed >= 2
                );
                if completed > 0 {
                    assert!(summary.contains(if unstructured {
                        "An independent completed opinion with no structured extraction."
                    } else {
                        "Attributed finding"
                    }));
                }
                if completed == 2 {
                    assert!(summary.contains("No findings reported. This is not proof"));
                }
                assert_eq!(
                    fake.starts.load(Ordering::SeqCst),
                    3,
                    "publication must not commission replacement reviews"
                );
                let reopened = AuditStore::open(controller.root.clone()).await?;
                assert_eq!(reopened.read_report(&report)?, diagnostic);
            }
        }
    }
    Ok(())
}
