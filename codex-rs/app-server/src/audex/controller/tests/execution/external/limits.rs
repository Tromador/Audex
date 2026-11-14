//! Stale accounting settings are inert; usage remains evidence, never admission.
use super::*;

#[test]
fn stale_client_limits_and_missing_estimates_never_block_operator_retry() -> anyhow::Result<()> {
    run_test_with_large_stack("m5-client-authority", || async {
        for missing in [false, true] {
            let temp = tempfile::tempdir()?;
            let server = responses::start_mock_server().await;
            let (controller, threads) = real_fixture(&temp, &server.uri()).await;
            let id = repairs::seed(
                &controller,
                &temp,
                json!({"single_stop":"pause_turn","omit_cost":missing}),
            )
            .await?;
            let store = controller.store().await?;
            let original = store.load_run_metadata(&id).await?;
            let raw = super::super::super::limits::retain_legacy_limits(
                store,
                &original,
                json!({"maxAttempts":1,"budgetUsd":"0.0001"}),
            )
            .await?;
            for ordinal in 1..=4 {
                let run = capture::settled(&controller, &id).await?;
                assert_eq!(run.attempt_ordinal, ordinal);
                lifecycle_qualification::reconcile_until(&controller, &run.operation_id).await?;
                let launch = receipt(temp.path(), "stream-prompt-receipt.json").await?;
                assert!(launch["max_budget_usd"].is_null());
                let read = api(
                    &controller,
                    "audit/attempt/read",
                    json!({
                    "runId":id,"assignmentId":run.assignment_id}),
                )
                .await
                .unwrap();
                assert_eq!(read["attempt"]["retryEligible"], true);
                assert!(read["attempt"].get("limits").is_none());
                assert_eq!(store.attempt_input(&run.operation_id).await?.0, raw);
                let events = store.activity(&run.assignment_id, 0, 256).await?;
                assert!(
                    events
                        .iter()
                        .any(|e| e.kind == "assistant_delta" || e.kind == "assistant_message")
                );
                if !missing {
                    assert!(
                        events
                            .iter()
                            .any(|e| e.payload.contains("estimated_cost_usd"))
                    );
                }
                let result = api(
                    &controller,
                    "audit/attempt/control",
                    json!({
                    "runId":id,"assignmentId":run.assignment_id,"action":"retry",
                    "actionKey":format!("operator-{ordinal}"),"reason":"retry preserved partial",
                    "authoriseRetry":true}),
                )
                .await
                .unwrap();
                assert!(result["replacementAssignmentId"].is_string());
            }
            controller.abort_audit(&id).await?;
            assert_eq!(store.list_run_ids("", 100).await?.len(), 1);
            assert_eq!(store.attempt_input(&original.operation_id).await?.0, raw);
            threads
                .shutdown_all_threads_bounded(Duration::from_secs(5))
                .await;
            store.close().await;
        }
        Ok(())
    })
}

#[test]
fn client_error_notices_do_not_discard_terminal_usage_receipts() -> anyhow::Result<()> {
    run_test_with_large_stack("m5-client-error-receipt", || async {
        for terminal_error in [false, true] {
            let temp = tempfile::tempdir()?;
            let server = responses::start_mock_server().await;
            let (controller, threads) = real_fixture(&temp, &server.uri()).await;
            let id = repairs::seed(
                &controller,
                &temp,
                json!({
                    "error_notice":true,"error_terminal":terminal_error,
                    "single_stop":"end_turn"
                }),
            )
            .await?;
            let run = capture::settled(&controller, &id).await?;
            let store = controller.store().await?;
            assert_eq!(
                run.assignment_state == AssignmentState::Succeeded,
                !terminal_error
            );
            let events = store.activity(&run.assignment_id, 0, 100).await?;
            assert!(
                events
                    .iter()
                    .any(|e| e.payload == "client_error_observed_awaiting_terminal")
            );
            assert!(
                !events
                    .iter()
                    .any(|e| e.payload.contains("PRIVATE_SENTINEL"))
            );
            if terminal_error {
                assert!(
                    events
                        .iter()
                        .any(|e| e.payload.contains("estimated_cost_usd"))
                );
            }
            controller.abort_audit(&id).await?;
            lifecycle_qualification::reconcile_until(&controller, &run.operation_id).await?;
            threads
                .shutdown_all_threads_bounded(Duration::from_secs(5))
                .await;
            store.close().await;
        }
        Ok(())
    })
}
