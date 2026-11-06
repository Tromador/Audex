//! Finished generations and cut-off tails through the real collector and store.
use super::*;

async fn partial_report(
    controller: &Arc<AuditController>,
    id: &str,
    run: &AuditRunRecord,
    expected_text: &str,
) -> anyhow::Result<()> {
    let store = controller.store().await?;
    assert!(matches!(
        run.assignment_state,
        AssignmentState::Failed | AssignmentState::Unknown
    ));
    assert_eq!(
        store.capture_state(&run.assignment_id).await?.disposition,
        "active"
    );
    assert!(store.result_extraction(&run.assignment_id).await?.is_none());
    assert!(
        store
            .client_terminal_result(&run.operation_id)
            .await?
            .is_none()
    );
    assert!(
        store.freeze_report(id).await.is_err(),
        "wanted failure needs an operator decision"
    );
    let activity = store.activity(&run.assignment_id, 0, 256).await?;
    assert!(activity.iter().any(|e| e.payload.contains(expected_text)));
    let receipts = activity
        .iter()
        .filter(|e| e.kind == "status")
        .filter_map(|e| serde_json::from_str::<Value>(&e.payload).ok())
        .filter(|v| v["client_terminal"] == "failed")
        .collect::<Vec<_>>();
    assert_eq!(
        receipts.len(),
        1,
        "one retained failed-terminal aggregate receipt"
    );
    let receipt = &receipts[0];
    assert!(
        receipt["terminal_id"]
            .as_str()
            .unwrap()
            .starts_with("repair-event-")
    );
    assert_eq!(
        receipt["usage"],
        json!({
            "input_tokens":55,"cache_read_input_tokens":13,"cache_creation_input_tokens":7,
            "output_tokens":35,"estimated_cost_usd":"0.0123"
        })
    );
    assert!(
        store
            .model_evidence(&run.assignment_id)
            .await?
            .iter()
            .all(|e| {
                e.state == "verified" && e.observed_model.as_deref() == Some("claude-haiku-5-5")
            })
    );
    assert!(!store.model_evidence(&run.assignment_id).await?.is_empty());
    controller.abort_audit(id).await?;
    let frozen = store.freeze_report(id).await?;
    assert!(frozen.reviewers[0].completed.is_none());
    assert!(frozen.reviewers[0].opinion.is_none());
    let facts = serde_json::to_value(&frozen)?;
    let facts = &facts["reviewers"][0]["provider_facts"];
    assert_eq!(facts["client_terminal"]["status"], "unavailable");
    let aggregates = facts["usage"]["value"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|v| {
            v["evidence_source"] == "claude_client_failed_terminal_aggregate_not_generation"
        })
        .collect::<Vec<_>>();
    assert_eq!(aggregates.len(), 1);
    assert_eq!(
        aggregates[0],
        &json!({
            "request_id":receipt["terminal_id"],
            "evidence_source":"claude_client_failed_terminal_aggregate_not_generation",
            "input_tokens":{"status":"known","value":55},
            "cached_input_tokens":{"status":"known","value":13},
            "output_tokens":{"status":"known","value":35},
            "reasoning_tokens":{"status":"unavailable","reason":"not observed"},
            "cost":{"status":"known","value":{"amount":"0.0123","currency":"USD","measured":false}}
        })
    );
    let before = serde_json::to_vec(&frozen)?;
    store.publish_report(id, 0).await?;
    let report = store.load_run(id).await?.report.unwrap();
    let bytes = store.read_report(&report)?;
    assert!(std::str::from_utf8(&bytes)?.contains(expected_text));
    assert!(
        std::str::from_utf8(&bytes)?
            .contains(r"claude\_client\_failed\_terminal\_aggregate\_not\_generation")
    );
    assert!(std::str::from_utf8(&bytes)?.contains("0.0123"));
    let reopened = AuditStore::open(controller.root.clone()).await?;
    assert!(
        reopened
            .activity(&run.assignment_id, 0, 256)
            .await?
            .iter()
            .any(|e| serde_json::from_str::<Value>(&e.payload).ok().as_ref() == Some(receipt))
    );
    assert_eq!(
        serde_json::to_vec(&reopened.freeze_report(id).await?)?,
        before
    );
    assert_eq!(reopened.read_report(&report)?, bytes);
    Ok(())
}

#[test]
fn successful_client_terminal_requires_a_finished_generation() -> anyhow::Result<()> {
    run_test_with_large_stack("finished-generation", || async {
        // pause_turn/success/false/pausing is the pinned 2.1.314 wire observation.
        // Neither JSON validity nor a contradictory success terminal completes it.
        for scenario in [
            json!({"single_stop":"pause_turn"}),
            json!({"single_stop":"pause_turn","single_structured":true}),
            json!({"single_stop":null}),
            json!({"single_stop":"max_tokens"}),
            json!({"single_stop":"tool_use"}),
            json!({"single_stop":"stop_sequence"}),
            json!({"single_stop":"unrecognised-stop"}),
            json!({"single_stop":"pause_turn","terminal_stop":"end_turn"}),
            json!({"single_stop":"end_turn","terminal_stop":"pause_turn"}),
        ] {
            let temp = tempfile::tempdir()?;
            let server = responses::start_mock_server().await;
            let (controller, threads) = real_fixture(&temp, &server.uri()).await;
            let id = seed(&controller, &temp, scenario.clone()).await?;
            let run = settled(&controller, &id).await?;
            let store = controller.store().await?;
            let failure_code: Option<String> =
                sqlx::query_scalar("SELECT failure_code FROM reviewer_assignments WHERE id=?")
                    .bind(&run.assignment_id)
                    .fetch_one(&store.pool_for_tests())
                    .await?;
            let capture_failure = store.capture_state(&run.assignment_id).await?.failure;
            assert!(
                failure_code.as_deref() == Some("external_final_generation_incomplete")
                    || capture_failure.as_deref() == Some("external_final_generation_incomplete"),
                "{scenario}: failure_code={failure_code:?}, capture_failure={capture_failure:?}"
            );
            assert!(notices(store, &run.assignment_id).await?.is_empty());
            let text = if scenario["single_structured"] == true {
                "Still paused"
            } else {
                "pausing"
            };
            partial_report(&controller, &id, &run, text).await?;
            one_prompt(&temp, &controller, &run.operation_id).await?;
            threads
                .shutdown_all_threads_bounded(Duration::from_secs(5))
                .await;
            println!(
                "unfinished terminal {scenario}: retained partial, no completed opinion or invented repair, explicit Abort, immutable report"
            );
        }
        Ok(())
    })
}

#[test]
fn cut_off_terminal_arriving_before_policy_stop_requires_validated_structure() -> anyhow::Result<()>
{
    run_test_with_large_stack("cutoff-terminal-first", || async {
        for (form, eligible) in [
            ("structured", true),
            ("fenced", true),
            ("fragment", false),
            ("wrong_identity", false),
        ] {
            let temp = tempfile::tempdir()?;
            let server = responses::start_mock_server().await;
            let (hooks, entered, release) = hold("client_policy_stop");
            let (controller, threads) = fixture_with_hooks(
                &temp,
                &server.uri(),
                CodexAuth::from_api_key("dummy"),
                false,
                Arc::new(|_, _| Box::pin(async { Ok(()) })),
                hooks,
            )
            .await;
            let id = seed(
                &controller,
                &temp,
                json!({"mode":"max_tokens","complete":true,
                "partial_start":true,"final_form":form}),
            )
            .await?;
            controller.drive(&id).await?;
            tokio::time::timeout(Duration::from_secs(10), entered.notified()).await?;
            // The internal stop transaction is held until after terminal processing,
            // result disposition, explicit operator decision (if needed) and publication.
            let run = settled(&controller, &id).await?;
            let store = controller.store().await?;
            assert_eq!(
                run.assignment_state == AssignmentState::Succeeded,
                eligible,
                "{form}"
            );
            let events = notices(store, &run.assignment_id).await?;
            assert_eq!(events.len(), 1);
            assert_eq!(events[0]["class"], "max_tokens_continuation");
            let activity = store.activity(&run.assignment_id, 0, 256).await?;
            assert!(
                activity
                    .iter()
                    .any(|e| e.payload == "Incomplete finding draft.")
            );
            if eligible {
                let extraction = store.result_extraction(&run.assignment_id).await?.unwrap();
                assert_eq!(
                    extraction.status,
                    if form == "fenced" {
                        codex_app_server_protocol::AuditExtractionStatus::DeterministicallyNormalised
                    } else {
                        codex_app_server_protocol::AuditExtractionStatus::ValidatedStructured
                    }
                );
                assert!(
                    store
                        .client_terminal_result(&run.operation_id)
                        .await?
                        .is_some()
                );
                store.freeze_report(&id).await?;
                store.publish_report(&id, 0).await?;
            } else {
                let failure_code: Option<String> =
                    sqlx::query_scalar("SELECT failure_code FROM reviewer_assignments WHERE id=?")
                        .bind(&run.assignment_id)
                        .fetch_one(&store.pool_for_tests())
                        .await?;
                let capture_failure = store.capture_state(&run.assignment_id).await?.failure;
                assert!(
                    failure_code.as_deref() == Some("external_incomplete_continuation")
                        || capture_failure.as_deref() == Some("external_incomplete_continuation"),
                    "{form}: failure_code={failure_code:?}, capture_failure={capture_failure:?}"
                );
                partial_report(
                    &controller,
                    &id,
                    &run,
                    if form == "fragment" {
                        "OK-final"
                    } else {
                        "Unbound tail"
                    },
                )
                .await?;
            }
            let frozen = store.freeze_report(&id).await?;
            assert_eq!(frozen.reviewers[0].completed.is_some(), eligible);
            let before = serde_json::to_vec(&frozen)?;
            let report = store.load_run(&id).await?.report.unwrap();
            let bytes = store.read_report(&report)?;
            assert!(std::str::from_utf8(&bytes)?.contains("client-initiated repair occurred"));
            release.add_permits(1);
            one_prompt(&temp, &controller, &run.operation_id).await?;
            assert_eq!(
                serde_json::to_vec(&store.freeze_report(&id).await?)?,
                before
            );
            assert_eq!(store.read_report(&report)?, bytes);
            let reopened = AuditStore::open(controller.root.clone()).await?;
            assert_eq!(
                serde_json::to_vec(&reopened.freeze_report(&id).await?)?,
                before
            );
            threads
                .shutdown_all_threads_bounded(Duration::from_secs(5))
                .await;
            println!(
                "cutoff terminal_first {form}: completed={eligible}; repair and original prefix retained; late stop/reopen cannot change report"
            );
        }
        Ok(())
    })
}
