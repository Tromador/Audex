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
