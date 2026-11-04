//! Personal F2 admission and coexistence; no public API route is enabled here.
use super::*;

#[test]
fn personal_pro_and_max_need_no_machine_lock_and_allow_api_configuration() -> anyhow::Result<()> {
    run_test_with_large_stack("c71-personal", || async {
        for tier in ["pro", "max"] {
            let temp = tempfile::tempdir()?;
            let server = responses::start_mock_server().await;
            let (controller, threads) = real_fixture(&temp, &server.uri()).await;
            let id = seed_stream(&controller, &temp, "valid", false).await?;
            let machine = controller.root.join("c71-system-policy");
            std::fs::remove_file(machine.join("managed-settings.json"))?;
            // These are precisely the machine surfaces the existing private API
            // preparation excludes; this subscription needs none of them.
            for name in [
                "managed-settings.json",
                "managed-mcp.json",
                "managed-settings.d",
            ] {
                assert!(!machine.join(name).try_exists()?);
            }
            let path = controller.root.join("reviewer-backends.json");
            let mut value: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
            let before: BackendSettings = serde_json::from_value(value.clone())?;
            value["claudeApi"] = json!({
                "executable":value["claudeSubscription"]["executable"],
                "clientVersion":"2.1.314", "upstream":"https://api.example.invalid",
                "credential":{"kind":"environment","reference":"AUDEX_DUMMY_API_KEY"},
                "runtimeRoot":value["claudeSubscription"]["runtimeRoot"]
            });
            let both: BackendSettings = serde_json::from_value(value)?;
            assert_eq!(
                before.identity(&AuditAuthenticationRoute::ClaudeSubscription)?,
                both.identity(&AuditAuthenticationRoute::ClaudeSubscription)?
            );
            std::fs::write(path, serde_json::to_vec(&both)?)?;
            std::fs::write(
                temp.path().join("policy-scenario.json"),
                json!({
                    "tier":tier,"display_tier":if tier == "pro" {"Claude Pro"} else {"Claude Max"},
                    "managed_settings":{},"setting_source":"User settings"
                })
                .to_string(),
            )?;
            let run = settled(&controller, &id).await?;
            assert_eq!(run.assignment_state, AssignmentState::Succeeded, "{tier}");
            let store = controller.store().await?;
            let launch = store.client_launch(&run.operation_id).await?.unwrap();
            assert!(launch.prompt_write_started_at_ms.is_some());
            assert_eq!(
                std::fs::read_to_string(temp.path().join("policy-control-count"))?,
                "5"
            );
            assert_eq!(
                receipt(temp.path(), "stream-prompt-receipt.json").await?["count"],
                1
            );
            gone(launch.pid.unwrap() as u32).await?;
            assert_no_private_files(controller.root.as_path())?;
            threads
                .shutdown_all_threads_bounded(Duration::from_secs(5))
                .await;
        }
        Ok(())
    })
}

#[test]
fn personal_name_and_empty_settings_do_not_admit_ambiguous_or_remote_policy() -> anyhow::Result<()>
{
    run_test_with_large_stack("c71-personal-refusal", || async {
        for mode in [
            "team_tier",
            "enterprise_tier",
            "missing_tier",
            "null_tier",
            "unknown_tier",
            "missing_user",
            "remote_source",
            "organisation_policy",
            "remote_pending",
        ] {
            let temp = tempfile::tempdir()?;
            let server = responses::start_mock_server().await;
            let (controller, threads) = real_fixture(&temp, &server.uri()).await;
            let id = seed_stream(&controller, &temp, "valid", false).await?;
            std::fs::remove_file(
                controller
                    .root
                    .join("c71-system-policy/managed-settings.json"),
            )?;
            std::fs::write(temp.path().join("policy-scenario.json"), json!({
                "mode":mode,"display_tier":"Claude Pro","managed_settings":{},"setting_source":"User settings"
            }).to_string())?;
            let run = settled(&controller, &id).await?;
            let store = controller.store().await?;
            let launch = store.client_launch(&run.operation_id).await?.unwrap();
            assert_eq!(run.assignment_state, AssignmentState::Failed, "{mode}");
            assert_eq!(
                store.submission_boundary(&run.operation_id).await?,
                SubmissionBoundary::Unsubmitted,
                "{mode}"
            );
            assert!(launch.prompt_write_started_at_ms.is_none(), "{mode}");
            assert!(
                !temp.path().join("stream-prompt-receipt.json").exists(),
                "{mode}"
            );
            gone(launch.pid.unwrap() as u32).await?;
            threads
                .shutdown_all_threads_bounded(Duration::from_secs(5))
                .await;
        }
        Ok(())
    })
}
