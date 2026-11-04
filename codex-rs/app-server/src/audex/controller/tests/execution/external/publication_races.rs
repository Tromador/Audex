//! External terminal contributions obey the existing Abort/publication writer order.
use super::capture::{seed_stream, settled};
use super::*;

#[test]
fn external_abort_and_publication_races_preserve_completed_evidence_in_both_orders()
-> anyhow::Result<()> {
    run_test_with_large_stack("c63-publication-order", || async {
        for abort_first in [false, true] {
            let temp = tempfile::tempdir()?;
            let server = responses::start_mock_server().await;
            let (controller, threads) = real_fixture(&temp, &server.uri()).await;
            let id = seed_stream(&controller, &temp, "valid", false).await?;
            let run = settled(&controller, &id).await?;
            assert_eq!(run.assignment_state, AssignmentState::Succeeded);
            let store = controller.store().await?;
            let original = store
                .client_terminal_result(&run.operation_id)
                .await?
                .unwrap();
            let (entered, release) =
                store.pause_transition_for_test(if abort_first { "abort" } else { "publish" });
            let (first_controller, first_store, first_id) =
                (controller.clone(), store.clone(), id.clone());
            let first = tokio::spawn(async move {
                if abort_first {
                    first_controller.abort_audit(&first_id).await
                } else {
                    first_store
                        .publish_report(&first_id, 0)
                        .await
                        .map(|_| ())
                        .map_err(anyhow::Error::from)
                }
            });
            tokio::time::timeout(Duration::from_secs(5), entered.notified()).await?;
            let (second_controller, second_store, second_id) =
                (controller.clone(), store.clone(), id.clone());
            let second = tokio::spawn(async move {
                if abort_first {
                    second_store
                        .publish_report(&second_id, 0)
                        .await
                        .map(|_| ())
                        .map_err(anyhow::Error::from)
                } else {
                    second_controller.abort_audit(&second_id).await
                }
            });
            tokio::task::yield_now().await;
            release.add_permits(1);
            first.await??;
            second.await??;
            let frozen = store.freeze_report(&id).await?;
            assert_eq!(
                frozen.outcome,
                if abort_first {
                    "aborted_by_operator"
                } else {
                    "success"
                }
            );
            assert!(frozen.reviewers[0].completed.is_some());
            let report = store.load_run(&id).await?.report.unwrap();
            super::lifecycle_qualification::reconcile_until(&controller, &run.operation_id).await?;
            assert_eq!(store.load_run(&id).await?.report, Some(report));
            assert_eq!(
                store
                    .client_terminal_result(&run.operation_id)
                    .await?
                    .unwrap(),
                original
            );
            assert_eq!(
                std::fs::read_to_string(temp.path().join("stream-launches"))?
                    .lines()
                    .count(),
                1
            );
            threads
                .shutdown_all_threads_bounded(Duration::from_secs(5))
                .await;
            println!(
                "C63 external Abort/publication order: abort_first={abort_first}; accepted contribution and report identity preserved; cleanup independent"
            );
        }
        Ok(())
    })
}
