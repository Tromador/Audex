use super::*;
use crate::audex::report::collection;
use crate::audex::snapshot::manifest::sha256_hex;
use crate::audex::store::admission::AdmissionIdentity;

#[tokio::test]
async fn native_format_four_freezes_policy_models_usage_and_late_receipts_separately()
-> anyhow::Result<()> {
    let (_temp, controller, fake, id, attempts) = super::super::attempts::setup().await?;
    let store = controller.store().await?;
    let attempt = &attempts[0];
    let _capture = super::super::attempts::capture(&controller, &fake, attempt).await?;
    let owner = AdmissionIdentity {
        run_id: id.clone(),
        assignment_id: attempt.assignment_id.clone(),
        operation_id: attempt.operation_id.clone(),
        execution_id: store
            .execution(&attempt.operation_id)
            .await?
            .context("execution")?
            .0,
        owner: controller.owner.clone(),
        owner_generation: attempt.owner_generation,
        run_stop_generation: store.run_stop_generation(&id).await?,
    };
    let now = chrono::Utc::now().timestamp_millis();
    store
        .admit_generation(&owner, "before-freeze", "native_http", now)
        .await?;
    assert!(
        store
            .mark_generation_send(&owner, "before-freeze", now)
            .await?
    );
    let usage = json!({"total_token_usage":{"input_tokens":12,"cached_input_tokens":2,"cache_write_input_tokens":0,"output_tokens":5,"reasoning_output_tokens":1,"total_tokens":17},
        "last_token_usage":{"input_tokens":12,"cached_input_tokens":2,"cache_write_input_tokens":0,"output_tokens":5,"reasoning_output_tokens":1,"total_tokens":17},"model_context_window":4096});
    store
        .execution_usage(&attempt.operation_id, &usage.to_string())
        .await?;
    for (ordinal, peer) in attempts.iter().enumerate() {
        store
            .control_attempt(
                &control(&id, peer, &format!("stop-{ordinal}"), AttemptAction::Cancel),
                None,
            )
            .await?;
    }
    let frozen = store.freeze_report(&id).await?;
    let before = serde_json::to_vec(&frozen)?;
    assert_eq!(frozen.format_version, 4);
    let value = serde_json::to_value(&frozen)?;
    let facts = &value["reviewers"][0]["provider_facts"];
    assert_eq!(facts["executor"]["value"], "native");
    assert_eq!(facts["route"]["value"], "daemon_configured_native");
    assert_eq!(
        facts["retained_bindings"]["value"]["snapshotId"],
        frozen.snapshot_id
    );
    assert_eq!(
        facts["native_usage"]["value"]["total_token_usage"]["total_tokens"],
        17
    );
    assert_eq!(facts["usage"]["status"], "unavailable");
    assert_eq!(
        facts["subscription"]["repair_occurrences"]["status"],
        "not_applicable"
    );
    assert_eq!(facts["ollama"]["model_digest"]["status"], "not_applicable");
    assert!(facts["receipts"][0]["confirmed_at_ms"].is_null());
    let expected = collection::render(&frozen)?;
    store
        .confirm_generation(
            &owner,
            "before-freeze",
            Some("late-provider-receipt"),
            now + 1,
        )
        .await?;
    let mut later = usage;
    later["total_token_usage"]["total_tokens"] = json!(999);
    store
        .execution_usage(&attempt.operation_id, &later.to_string())
        .await?;
    assert_eq!(
        store.generation_records(&attempt.assignment_id).await?[0].confirmed_at_ms,
        Some(now + 1)
    );
    assert_eq!(
        serde_json::to_vec(&store.freeze_report(&id).await?)?,
        before
    );
    let reopened = AuditStore::open(controller.root.clone()).await?;
    let report = record(reopened.publish_report(&id, 0).await?);
    assert_eq!(report.id, expected.id);
    assert_eq!(reopened.read_report(&report)?, expected.bytes);
    assert_eq!(
        reopened.load_run(&id).await?.outcome.as_deref(),
        Some("no_completed_review")
    );
    assert!(!String::from_utf8(expected.bytes)?.contains("late-provider-receipt"));
    Ok(())
}

#[tokio::test]
async fn frozen_format_three_recovers_before_render_and_after_candidate_without_upgrade()
-> anyhow::Result<()> {
    for candidate_exists in [false, true] {
        let (_temp, controller, fake, id, attempts) = super::super::attempts::setup().await?;
        let store = controller.store().await?;
        for attempt in &attempts {
            complete(&controller, attempt, false).await?;
        }
        let mut input = store.freeze_report(&id).await?;
        // Reconstruct the pre-upgrade writer in this isolated fixture. The real
        // pre-upgrade golden inputs independently pin the old bytes and digest.
        input.format_version = 3;
        for reviewer in &mut input.reviewers {
            reviewer.provider_facts = None;
        }
        let raw = serde_json::to_string(&input)?;
        let expected = collection::render(&input)?;
        sqlx::query("UPDATE audit_report_selections SET input_json=?,input_digest=? WHERE id=?")
            .bind(&raw)
            .bind(sha256_hex(raw.as_bytes()))
            .bind(&input.selection_id)
            .execute(&store.pool_for_tests())
            .await?;
        if candidate_exists {
            crate::audex::store::publish_immutable(
                controller.root.as_path(),
                &format!("reports/{id}/{}.md", expected.input_digest),
                &expected.bytes,
                &expected.content_digest,
            )?;
        }
        let reopened = AuditStore::open(controller.root.clone()).await?;
        assert_eq!(
            serde_json::to_string(&reopened.freeze_report(&id).await?)?,
            raw
        );
        let report = record(reopened.publish_report(&id, 0).await?);
        assert_eq!(report.id, expected.id);
        assert_eq!(reopened.read_report(&report)?, expected.bytes);
        let version: i64 = sqlx::query_scalar("SELECT format_version FROM reports WHERE id=?")
            .bind(&report.id)
            .fetch_one(&store.pool_for_tests())
            .await?;
        assert_eq!(version, 3);
        assert_eq!(record(reopened.publish_report(&id, 0).await?), report);
        assert_eq!(fake.starts.load(Ordering::SeqCst), 3);
    }
    Ok(())
}
