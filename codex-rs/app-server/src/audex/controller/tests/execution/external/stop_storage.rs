//! A committed stop does not depend on a second successful store read to signal local handles.
use super::*;

#[test]
fn durable_stop_signals_local_child_even_when_launch_receipt_reads_fail() -> anyhow::Result<()> {
    run_test_with_large_stack("c63-stop-storage", || async {
        let temp = tempfile::tempdir()?;
        let server = responses::start_mock_server().await;
        let (controller, threads) = real_fixture(&temp, &server.uri()).await;
        let id = capture::seed_stream(&controller, &temp, "valid", true).await?;
        controller.drive(&id).await?;
        let receipt = receipt(temp.path(), "stream-prompt-receipt.json").await?;
        let store = controller.store().await?;
        let run = store.load_run(&id).await?;
        store.abort_run(&id, &[]).await?;
        sqlx::query("ALTER TABLE audit_client_launches RENAME TO c63_unavailable_launches")
            .execute(&store.pool_for_tests())
            .await?;
        controller
            .dispatch
            .request_interrupt(run.operation_id.clone())
            .await;
        let stopped = gone(receipt["pid"].as_u64().unwrap() as u32).await;
        sqlx::query("ALTER TABLE c63_unavailable_launches RENAME TO audit_client_launches")
            .execute(&store.pool_for_tests())
            .await?;
        stopped?;
        super::lifecycle_qualification::reconcile_until(&controller, &run.operation_id).await?;
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
            "C63 committed stop: launch receipt read unavailable; local owner signalled without read/reap dependency; receipt recovered later; one prompt"
        );
        Ok(())
    })
}
