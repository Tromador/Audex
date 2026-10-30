use super::*;

#[tokio::test]
async fn damaged_reporting_work_does_not_starve_unrelated_dispatch() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let (controller, fake, _) = fixture(&temp).await;
    let bad = start(&controller, "damaged-reporting").await;
    controller.drive(&bad).await?;
    let store = controller.store().await?;
    let run = store.load_run(&bad).await?;
    fake.replace(&run.operation_id, WorkState::Completed(empty_result(&run)));
    controller.drive(&bad).await?;
    let run = store.load_run(&bad).await?;
    assert_eq!(run.phase, RunPhase::Reporting);
    let path = controller.root.join(run.result_path.expect("raw path"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path.as_path(), std::fs::Permissions::from_mode(0o644))?;
    }
    std::fs::write(path.as_path(), b"damaged raw result")?;
    let good = start(&controller, "independent-reporting").await;
    controller.recover_runs(true).await?;
    assert_eq!(
        store.load_run_metadata(&bad).await?.phase,
        RunPhase::Reporting
    );
    assert_eq!(
        store.load_run(&good).await?.operation_state,
        OperationState::Accepted
    );
    assert_eq!(fake.starts.load(Ordering::SeqCst), 2);
    assert!(
        api(&controller, "audit/read", json!({"runId":bad}))
            .await
            .is_err()
    );
    assert!(
        controller.polling.lock().await[&bad].2 > 0,
        "the damaged run is backed off rather than retried every worker tick"
    );
    Ok(())
}

#[tokio::test]
async fn capabilities_and_listing_do_not_open_retained_artefacts() -> anyhow::Result<()> {
    for corrupt_report in [false, true] {
        let temp = tempfile::tempdir()?;
        let (controller, fake, _) = fixture(&temp).await;
        let capability = api(&controller, "audit/capabilities", json!({}))
            .await
            .expect("capabilities");
        assert_eq!(capability["protocolVersion"], 2);
        assert!(
            !controller
                .root
                .join("audit-state.sqlite3")
                .as_path()
                .exists()
        );
        let bad = start(&controller, "damaged").await;
        controller.drive(&bad).await?;
        let store = controller.store().await?;
        let run = store.load_run(&bad).await?;
        fake.replace(&run.operation_id, WorkState::Completed(empty_result(&run)));
        controller.drive(&bad).await?;
        controller.drive(&bad).await?;
        let complete = store.load_run(&bad).await?;
        let good = start(&controller, "unrelated").await;
        let relative = if corrupt_report {
            complete.report.expect("report").relative_path
        } else {
            complete.result_path.expect("result")
        };
        let path = controller.root.join(relative);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path.as_path(), std::fs::Permissions::from_mode(0o644))?;
        }
        std::fs::write(path.as_path(), b"damaged retained bytes")?;
        let listed = api(&controller, "audit/list", json!({}))
            .await
            .expect("metadata list");
        assert_eq!(listed["runs"].as_array().expect("runs").len(), 2);
        for run in listed["runs"].as_array().expect("runs") {
            assert!(run["report"].is_null() && run["rawResult"].is_null());
        }
        assert!(
            api(&controller, "audit/read", json!({"runId":bad}))
                .await
                .is_err()
        );
        assert!(
            api(&controller, "audit/read", json!({"runId":good}))
                .await
                .is_ok()
        );
        assert!(
            api(&controller, "audit/capabilities", json!({}))
                .await
                .is_ok()
        );
        controller.drive(&good).await?;
        assert_eq!(fake.starts.load(Ordering::SeqCst), 2);
    }
    Ok(())
}

#[tokio::test]
async fn newest_first_pagination_is_stable_with_ties_and_new_insertions() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let (controller, _, _) = fixture(&temp).await;
    let a = start(&controller, "a").await;
    let b = start(&controller, "b").await;
    let c = start(&controller, "c").await;
    let store = controller.store().await?;
    sqlx::query("UPDATE audit_runs SET created_at_ms=10")
        .execute(&store.pool_for_tests())
        .await?;
    let first = api(&controller, "audit/list", json!({"limit":2}))
        .await
        .expect("page one");
    assert_eq!(first["runs"][0]["runId"], c);
    assert_eq!(first["runs"][1]["runId"], b);
    assert_eq!(first["nextCursor"], b);
    let d = start(&controller, "d").await;
    let next = api(&controller, "audit/list", json!({"limit":2,"after":b}))
        .await
        .expect("older page");
    assert_eq!(next["runs"].as_array().expect("runs").len(), 1);
    assert_eq!(next["runs"][0]["runId"], a);
    assert!(next["nextCursor"].is_null());
    assert_eq!(
        api(&controller, "audit/list", json!({"limit":1}))
            .await
            .expect("newest")["runs"][0]["runId"],
        d
    );
    assert!(
        api(&controller, "audit/list", json!({"after":"not-a-cursor"}))
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn committed_start_errors_and_mutable_preflight_retries_retain_original_identity()
-> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let (controller, fake, _) = fixture(&temp).await;
    controller.fail_start_response.store(true, Ordering::SeqCst);
    let params = json!({"requestKey":"lost-ack","source":{"type":"snippet","name":"example.rs","content":"fn main() {}\n"},"brief":"Static audit","provider":controller.config.model_provider_id,"model":"audit-test"});
    let error = api(&controller, "audit/start", params.clone())
        .await
        .expect_err("injected post-commit error");
    let data = error.data.expect("durable recovery evidence");
    assert_eq!(data["reason"], "auditRunRetained");
    let id = data["runId"].as_str().expect("run ID");
    controller.drive(id).await?;
    let mut changed = (*controller.config).clone();
    changed.model_providers.clear();
    let mut model = codex_models_manager::model_info::model_info_from_slug("audit-test");
    model.display_name = "Catalogue changed after the original request".to_string();
    let models = Arc::new(StaticModelsManager::new(
        None,
        ModelsResponse {
            models: vec![model],
        },
        ModelOrigin::ExplicitConfig,
    ));
    let recovered = AuditController::new(Arc::new(changed), models, fake.clone(), Arc::new(|_| {}));
    let retry = api(&recovered, "audit/start", params.clone())
        .await
        .expect("retained request avoids mutable preflight");
    assert_eq!(retry["run"]["runId"], id);
    assert_eq!(fake.starts.load(Ordering::SeqCst), 1);
    for (field, value) in [
        ("brief", json!("Different audit")),
        ("model", json!("different-model")),
        ("provider", json!("different-provider")),
        ("reasoningEffort", json!("high")),
        (
            "source",
            json!({"type":"snippet","name":"example.rs","content":"different source"}),
        ),
        ("audexGuidance", json!("Different captured guidance")),
    ] {
        let mut conflicting = params.clone();
        conflicting[field] = value;
        let conflict = api(&recovered, "audit/start", conflicting)
            .await
            .expect_err("different request identity");
        let conflict = conflict.data.expect("explicit conflict");
        assert_eq!(conflict["reason"], "auditIdempotencyConflict", "{field}");
        for identifier in ["runId", "assignmentId", "operationId"] {
            assert!(conflict.get(identifier).is_none(), "{field}: {identifier}");
        }
    }
    assert_eq!(
        controller.store().await?.list_run_ids("", 100).await?.len(),
        1
    );
    // Older schema rows cannot recover a discarded distinction between requested
    // defaults and resolved settings. Do not claim a legacy key proves identity.
    sqlx::query("UPDATE audit_inputs SET request_identity_digest=NULL")
        .execute(&controller.store().await?.pool_for_tests())
        .await?;
    let legacy = api(&recovered, "audit/start", params)
        .await
        .expect_err("legacy comparison remains conservative");
    assert!(
        legacy
            .data
            .as_ref()
            .is_none_or(|data| data.get("runId").is_none())
    );
    assert_eq!(fake.starts.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn recovery_backoff_and_progress_deduplication_do_not_delay_explicit_cancellation()
-> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let (controller, fake, notifications) = fixture(&temp).await;
    let id = start(&controller, "backoff").await;
    controller.recover_runs(true).await?;
    let store = controller.store().await?;
    let run = store.load_run(&id).await?;
    fake.work.lock().expect("work").remove(&run.operation_id);
    let count = notifications.lock().expect("notifications").len();
    for _ in 0..10 {
        controller.recover_runs(true).await?;
        controller.progress(&id).await?;
    }
    assert_eq!(notifications.lock().expect("notifications").len(), count);
    assert_eq!(fake.observations.load(Ordering::SeqCst), 0);
    for _ in 0..7 {
        controller
            .polling
            .lock()
            .await
            .get_mut(&id)
            .expect("schedule")
            .1 = tokio::time::Instant::now();
        controller.recover_runs(true).await?;
    }
    let (_, due, attempts) = controller.polling.lock().await[&id];
    assert!(attempts >= 6 && due.duration_since(tokio::time::Instant::now()).as_secs() >= 59);
    let count = fake.observations.load(Ordering::SeqCst);
    for _ in 0..10 {
        controller.recover_runs(true).await?;
    }
    assert_eq!(fake.observations.load(Ordering::SeqCst), count);
    api(&controller, "audit/cancel", json!({"runId":id}))
        .await
        .expect("durable cancel");
    controller.recover_runs(true).await?;
    assert_eq!(fake.observations.load(Ordering::SeqCst), count + 1);
    assert_eq!(fake.starts.load(Ordering::SeqCst), 1);
    assert!(store.load_run(&id).await?.observation_pending);
    Ok(())
}
