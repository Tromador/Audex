//! F2-1: client refresh is allowed before the per-reviewer metadata binding.
use super::*;

async fn share_profile(
    controller: &AuditController,
    id: &str,
    profile: &Path,
) -> anyhow::Result<()> {
    let path = controller.root.join("reviewer-backends.json");
    let mut value: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
    value["claudeSubscription"]["profile"] = json!(profile);
    let settings: BackendSettings = serde_json::from_value(value)?;
    std::fs::write(path, serde_json::to_vec(&settings)?)?;
    let store = controller.store().await?;
    let run = store.load_run(id).await?;
    let mut input = RetainedInput::decode(&store.attempt_input(&run.operation_id).await?.0)?;
    input
        .bindings
        .as_mut()
        .unwrap()
        .backend_configuration_identity = settings.identity(&input.route())?;
    let raw = serde_json::to_string(&input)?;
    let digest = sha256_hex(raw.as_bytes());
    let mut tx = store.pool_for_tests().begin().await?;
    sqlx::query(
        "UPDATE audit_attempt_inputs SET request_json=?,input_digest=? WHERE assignment_id=?",
    )
    .bind(raw)
    .bind(&digest)
    .bind(&run.assignment_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE operations SET input_digest=? WHERE id=?")
        .bind(digest)
        .bind(&run.operation_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

async fn assert_outcome(
    controller: &AuditController,
    temp: &tempfile::TempDir,
    run: &AuditRunRecord,
    submitted: bool,
) -> anyhow::Result<()> {
    let store = controller.store().await?;
    let launch = store.client_launch(&run.operation_id).await?.unwrap();
    if submitted {
        assert_eq!(run.assignment_state, AssignmentState::Succeeded);
        assert_eq!(
            store.submission_boundary(&run.operation_id).await?,
            SubmissionBoundary::Accepted
        );
        assert!(launch.prompt_write_started_at_ms.is_some());
        assert!(
            launch.client_accepted_at_ms.is_some(),
            "subscription init billing observation must pass"
        );
        assert_eq!(
            receipt(temp.path(), "stream-prompt-receipt.json").await?["count"],
            1
        );
    } else {
        assert_eq!(run.assignment_state, AssignmentState::Failed);
        assert_eq!(
            store.submission_boundary(&run.operation_id).await?,
            SubmissionBoundary::Unsubmitted
        );
        assert!(launch.prompt_write_started_at_ms.is_none());
        assert!(!temp.path().join("stream-prompt-receipt.json").exists());
    }
    gone(launch.pid.unwrap() as u32).await?;
    // Neither a successful refresh nor a later refusal authorises replay.
    for _ in 0..3 {
        controller.drive(&run.run_id).await?;
    }
    assert_eq!(
        store
            .client_launch(&run.operation_id)
            .await?
            .unwrap()
            .execution_id,
        launch.execution_id
    );
    assert_no_private_files(controller.root.as_path())?;
    Ok(())
}

#[test]
fn refresh_during_usage_preserves_tier_policy_and_final_binding_checks() -> anyhow::Result<()> {
    run_test_with_large_stack("c71-refresh", || async {
        for mode in [
            "valid",
            "missing_tier",
            "unknown_tier",
            "null_tier",
            "team_tier",
            "credential_changed_after_usage",
            "policy_changed_during_usage",
        ] {
            let temp = tempfile::tempdir()?;
            let server = responses::start_mock_server().await;
            let (controller, threads) = real_fixture(&temp, &server.uri()).await;
            let id = seed_stream(&controller, &temp, "valid", false).await?;
            let credential = temp.path().join("subscription-profile/.credentials.json");
            std::fs::write(&credential, b"dummy-expiring-token")?;
            std::fs::set_permissions(&credential, std::fs::Permissions::from_mode(0o600))?;
            use std::os::unix::fs::MetadataExt;
            let identity = |metadata: std::fs::Metadata| {
                (metadata.ino(), metadata.ctime(), metadata.ctime_nsec())
            };
            let before_refresh = identity(std::fs::symlink_metadata(&credential)?);
            std::fs::write(
                temp.path().join("policy-scenario.json"),
                json!({
                    "mode":mode,"refresh_credential":true
                })
                .to_string(),
            )?;
            let run = settled(&controller, &id).await?;
            assert_ne!(
                identity(std::fs::symlink_metadata(&credential)?),
                before_refresh,
                "fixture must replace the credential"
            );
            assert_outcome(&controller, &temp, &run, mode == "valid").await?;
            threads
                .shutdown_all_threads_bounded(Duration::from_secs(5))
                .await;
        }
        Ok(())
    })
}

#[test]
fn concurrent_reviewers_bind_independently_after_shared_profile_refresh() -> anyhow::Result<()> {
    run_test_with_large_stack("c71-shared-refresh", || async {
        for refresh_after_first_binding in [false, true] {
            let a = tempfile::tempdir()?;
            let b = tempfile::tempdir()?;
            let server = responses::start_mock_server().await;
            let (first, first_threads) = real_fixture(&a, &server.uri()).await;
            let (second, second_threads) = real_fixture(&b, &server.uri()).await;
            let first_id = seed_stream(&first, &a, "valid", false).await?;
            let second_id = seed_stream(&second, &b, "valid", false).await?;
            let profile = a.path().join("subscription-profile");
            share_profile(&second, &second_id, &profile).await?;
            let credential = profile.join(".credentials.json");
            std::fs::write(&credential, b"dummy-shared-expiring-token")?;
            std::fs::set_permissions(&credential, std::fs::Permissions::from_mode(0o600))?;
            std::fs::write(
                a.path().join("policy-scenario.json"),
                json!({
                    "refresh_credential":true,"hold_usage":!refresh_after_first_binding,
                    "hold_settings":refresh_after_first_binding
                })
                .to_string(),
            )?;
            std::fs::write(
                b.path().join("policy-scenario.json"),
                json!({
                    "refresh_credential":true,"hold_usage":true
                })
                .to_string(),
            )?;
            let (driver, id) = (first.clone(), first_id.clone());
            let first_task = tokio::spawn(async move { settled(&driver, &id).await });
            let first_gate = if refresh_after_first_binding {
                "settings"
            } else {
                "usage"
            };
            receipt(a.path(), &format!("{first_gate}.ready")).await?;
            let (driver, id) = (second.clone(), second_id.clone());
            let second_task = tokio::spawn(async move { settled(&driver, &id).await });
            receipt(b.path(), "usage.ready").await?;
            // Both refreshes are complete. In the first schedule neither reviewer
            // has bound yet. In the second, reviewer one's snapshot must stay fixed.
            std::fs::write(b.path().join("usage.release"), b"go")?;
            let second_run = second_task.await??;
            std::fs::write(a.path().join(format!("{first_gate}.release")), b"go")?;
            let first_run = first_task.await??;
            assert_outcome(&first, &a, &first_run, !refresh_after_first_binding).await?;
            assert_outcome(&second, &b, &second_run, true).await?;
            first_threads
                .shutdown_all_threads_bounded(Duration::from_secs(5))
                .await;
            second_threads
                .shutdown_all_threads_bounded(Duration::from_secs(5))
                .await;
        }
        Ok(())
    })
}
