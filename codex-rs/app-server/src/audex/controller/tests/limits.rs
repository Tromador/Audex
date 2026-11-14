//! Removed ceilings cannot govern retained audits or operator-directed retries.
use super::*;

pub(super) async fn retain_legacy_limits(
    store: &AuditStore,
    run: &AuditRunRecord,
    limits: Value,
) -> anyhow::Result<String> {
    let (raw, _, _, _) = store.attempt_input(&run.operation_id).await?;
    let mut input: Value = serde_json::from_str(&raw)?;
    input["operatorLimits"] = limits;
    let raw = serde_json::to_string(&input)?;
    let digest = sha256_hex(raw.as_bytes());
    sqlx::query(
        "UPDATE audit_attempt_inputs SET request_json=?,input_digest=? WHERE assignment_id=?",
    )
    .bind(&raw)
    .bind(&digest)
    .bind(&run.assignment_id)
    .execute(&store.pool_for_tests())
    .await?;
    sqlx::query("UPDATE operations SET input_digest=? WHERE id=?")
        .bind(digest)
        .bind(&run.operation_id)
        .execute(&store.pool_for_tests())
        .await?;
    Ok(raw)
}

#[tokio::test]
async fn stale_ceilings_allow_one_automatic_replacement_and_repeated_operator_retries()
-> anyhow::Result<()> {
    for limits in [
        json!({"maxAttempts":1,"budgetUsd":"0.0001"}),
        json!({"maxAttempts":0,"budgetUsd":"obsolete-invalid-value"}),
    ] {
        let (_temp, controller, fake, id, records) = attempts::setup().await?;
        let store = controller.store().await?;
        let mut current = records[0].clone();
        let raw = retain_legacy_limits(store, &current, limits).await?;
        for (obsolete, current_status) in [
            ("configured_reviewer_limit_blocks_replacement", None),
            (
                "automatic_recovery_refused_retained_policy_or_limit",
                Some("automatic_recovery_revalidation_required"),
            ),
        ] {
            store
                .set_recovery_status(&current.assignment_id, obsolete)
                .await?;
            assert_eq!(
                store
                    .recovery_status(&current.assignment_id)
                    .await?
                    .as_deref(),
                current_status
            );
        }
        let decoded = crate::audex::retained::RetainedInput::decode(&raw)?;
        assert!(
            serde_json::to_value(decoded)?
                .get("operatorLimits")
                .is_none()
        );
        let original = current.clone();
        for ordinal in 1..=6 {
            current = store.run_for_operation(&current.operation_id).await?;
            let _capture = attempts::capture(&controller, &fake, &current).await?;
            fake.replace(
                &current.operation_id,
                WorkState::Failed("stopped empty".into()),
            );
            store.mark_execution_stopped(&current.operation_id).await?;
            controller.drive_operation(&current.operation_id).await?;
            controller.drive_operation(&current.operation_id).await?;
            if ordinal > 1 {
                assert_eq!(store.attempts(&id).await?.len(), ordinal as usize + 2);
                let read = api(
                    &controller,
                    "audit/attempt/read",
                    json!({
                        "runId":id,"assignmentId":current.assignment_id
                    }),
                )
                .await
                .unwrap();
                assert_eq!(read["attempt"]["retryEligible"], true);
                assert!(read["attempt"].get("limits").is_none());
                assert_eq!(
                    read["attempt"]["recoveryStatus"],
                    "automatic_recovery_exhausted_operator_investigation_required"
                );
                let retried = api(&controller, "audit/attempt/control", json!({
                    "runId":id,"assignmentId":current.assignment_id,"action":"retry",
                    "actionKey":format!("explicit-{ordinal}"),"reason":"continue unfinished review",
                    "authoriseRetry":true
                })).await.unwrap();
                assert!(retried["replacementAssignmentId"].is_string());
            }
            current = store
                .attempts(&id)
                .await?
                .into_iter()
                .find(|r| r.current && r.reviewer_id == original.reviewer_id)
                .unwrap();
            assert_eq!(current.attempt_ordinal, ordinal + 1);
            assert_eq!(current.snapshot_id, original.snapshot_id);
            assert_eq!(current.provider, original.provider);
            assert_eq!(current.model, original.model);
            assert_eq!(store.attempt_input(&current.operation_id).await?.0, raw);
            controller.drive_operation(&current.operation_id).await?;
        }
        assert_eq!(store.list_run_ids("", 100).await?.len(), 1);
        assert_eq!(store.attempt_input(&original.operation_id).await?.0, raw);
    }
    Ok(())
}

#[test]
fn reviewer_configuration_has_no_optional_ceilings() -> anyhow::Result<()> {
    let selection = json!({"provider":"openai","model":"audit-test"});
    let decoded: codex_app_server_protocol::AuditReviewerSelection =
        serde_json::from_value(selection.clone())?;
    assert!(serde_json::to_value(decoded)?.get("limits").is_none());
    for limits in [json!({"maxAttempts":1}), json!({"budgetUsd":"1.00"})] {
        let mut obsolete = selection.clone();
        obsolete["limits"] = limits;
        assert!(
            serde_json::from_value::<codex_app_server_protocol::AuditReviewerSelection>(obsolete)
                .is_err()
        );
    }
    Ok(())
}
