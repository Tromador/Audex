//! C7.1: real preparation/owned pipes, deterministic policy replies, no inference.
use super::capture::{assert_no_private_files, seed_stream, settled};
use super::*;
mod credential_refresh;
mod personal;

#[test]
fn incomplete_or_incompatible_handshakes_refuse_before_submission() -> anyhow::Result<()> {
    run_test_with_large_stack("c71-handshake", || async {
        for mode in [
            "missing_authority",
            "missing_user",
            "team_tier",
            "enterprise_tier",
            "unknown_tier",
            "null_tier",
            "missing_tier",
            "credential_changed_after_usage",
            "policy_changed_during_usage",
            "remote_source",
            "organisation_policy",
            "parent",
            "load_error",
            "remote_env",
            "gateway",
            "wrong_client_pid",
            "managed_hook",
            "remote_pending",
            "remote_stale",
            "remote_failed",
            "windows_policy",
            "wrong_response_id",
            "early_eof",
            "malformed",
            "oversized",
            "timeout",
            "wrong_client_version",
            "api_account",
            "wrong_model",
            "memory_enabled",
        ] {
            let temp = tempfile::tempdir()?;
            let server = responses::start_mock_server().await;
            let (controller, threads) = real_fixture(&temp, &server.uri()).await;
            let id = seed_external(&controller, &temp, mode).await?;
            std::fs::write(
                temp.path().join("policy-scenario.json"),
                json!({"mode":mode}).to_string(),
            )?;
            controller.drive(&id).await?;
            let store = controller.store().await?;
            let run = store.load_run(&id).await?;
            let launch = store.client_launch(&run.operation_id).await?.unwrap();
            assert_eq!(run.assignment_state, AssignmentState::Failed, "{mode}");
            assert_eq!(
                store.submission_boundary(&run.operation_id).await?,
                SubmissionBoundary::Unsubmitted,
                "{mode}"
            );
            assert!(launch.prompt_write_started_at_ms.is_none(), "{mode}");
            gone(launch.pid.unwrap() as u32).await?;
            let directory = Path::new(&launch.mcp_config_path).parent().unwrap();
            assert!(!directory.join("receipt.json").exists(), "{mode}");
            assert_no_private_files(controller.root.as_path())?;
            for _ in 0..3 {
                controller.drive(&id).await?;
            }
            assert_eq!(
                store
                    .client_launch(&run.operation_id)
                    .await?
                    .unwrap()
                    .execution_id,
                launch.execution_id
            );
            assert_eq!(
                std::fs::read_to_string(directory.join("spawn-count"))?
                    .lines()
                    .count(),
                1
            );
            threads
                .shutdown_all_threads_bounded(Duration::from_secs(5))
                .await;
        }
        Ok(())
    })
}

#[test]
fn local_policy_refusal_precedes_client_launch() -> anyhow::Result<()> {
    run_test_with_large_stack("c71-preparation", || async {
        for mode in [
            "org_pin",
            "managed_mcp",
            "managed_env",
            "drop_in",
            "profile",
            "remote_cache",
            "windows_query_unreadable",
            "windows_malformed",
            "windows_incompatible",
            "windows_org_pin",
            "windows_disabled_unreadable",
        ] {
            let temp = tempfile::tempdir()?;
            let server = responses::start_mock_server().await;
            let (controller, threads) = real_fixture(&temp, &server.uri()).await;
            let id = seed_external(&controller, &temp, mode).await?;
            let machine = controller.root.join("c71-system-policy");
            let profile = temp.path().join("subscription-profile");
            match mode {
                "org_pin" => std::fs::write(
                    machine.join("managed-settings.json"),
                    r#"{"forceLoginOrgUUID":"dummy-org"}"#,
                )?,
                "managed_mcp" => std::fs::write(machine.join("managed-mcp.json"), b"{}")?,
                "managed_env" => std::fs::write(
                    machine.join("managed-settings.json"),
                    r#"{"forceRemoteSettingsRefresh":true,"env":{"ANTHROPIC_API_KEY":"dummy"}}"#,
                )?,
                "drop_in" => {
                    std::fs::create_dir(machine.join("managed-settings.d"))?;
                    std::fs::write(
                        machine.join("managed-settings.d/10.json"),
                        r#"{"disableAllHooks":false}"#,
                    )?;
                }
                "profile" => {
                    std::fs::set_permissions(&profile, std::fs::Permissions::from_mode(0o755))?
                }
                "remote_cache" => std::fs::write(
                    profile.join("remote-settings.json"),
                    r#"{"forceLoginMethod":"gateway"}"#,
                )?,
                "windows_query_unreadable" => {
                    std::fs::write(machine.join("windows-registry"), b"dummy")?
                }
                "windows_malformed"
                | "windows_incompatible"
                | "windows_org_pin"
                | "windows_disabled_unreadable" => {
                    let directory = machine.join("windows-policy");
                    std::fs::create_dir(&directory)?;
                    let bytes = match mode {
                        "windows_malformed" => "{",
                        "windows_incompatible" => {
                            r#"{"wslInheritsWindowsSettings":true,"forceRemoteSettingsRefresh":true,"env":{"ANTHROPIC_API_KEY":"dummy"}}"#
                        }
                        "windows_org_pin" => {
                            r#"{"wslInheritsWindowsSettings":true,"forceLoginOrgUUID":"dummy-org"}"#
                        }
                        _ => r#"{"wslInheritsWindowsSettings":false}"#,
                    };
                    std::fs::write(directory.join("managed-settings.json"), bytes)?;
                    if mode == "windows_disabled_unreadable" {
                        std::fs::write(directory.join("managed-settings.d"), b"not a directory")?;
                    }
                }
                _ => unreachable!(),
            }
            controller.drive(&id).await?;
            let store = controller.store().await?;
            let run = store.load_run(&id).await?;
            assert_eq!(run.assignment_state, AssignmentState::Failed, "{mode}");
            assert_eq!(
                store.submission_boundary(&run.operation_id).await?,
                SubmissionBoundary::Unsubmitted
            );
            assert!(
                store.client_launch(&run.operation_id).await?.is_none(),
                "{mode}"
            );
            assert!(!temp.path().join("policy-control-count").exists());
            threads
                .shutdown_all_threads_bounded(Duration::from_secs(5))
                .await;
        }
        Ok(())
    })
}

#[test]
fn policy_and_backend_changes_after_handshake_are_fenced() -> anyhow::Result<()> {
    run_test_with_large_stack("c71-final-binding", || async {
        for mode in [
            "profile",
            "credential",
            "machine",
            "windows",
            "backend",
            "revoke",
        ] {
            let temp = tempfile::tempdir()?;
            let server = responses::start_mock_server().await;
            let path = temp.path().to_owned();
            let hooks = crate::audex::dispatch::DispatchTestHooks {
                checkpoint: Some(Arc::new(move |stage| {
                    let path = path.clone();
                    Box::pin(async move {
                        if stage == "policy_verified" {
                            let root = path.join("audex");
                            match mode {
                                "profile" => {
                                    let file = path.join("subscription-profile/settings.json");
                                    let mut settings: Value =
                                        serde_json::from_slice(&std::fs::read(&file).unwrap())
                                            .unwrap();
                                    settings["theme"] = "light".into();
                                    std::fs::write(file, settings.to_string()).unwrap();
                                }
                                "credential" => {
                                    let file = path.join("subscription-profile/.credentials.json");
                                    std::fs::write(&file, b"dummy-refresh-metadata").unwrap();
                                    std::fs::set_permissions(
                                        file,
                                        std::fs::Permissions::from_mode(0o600),
                                    )
                                    .unwrap();
                                }
                                "machine" => std::fs::write(
                                    root.join("c71-system-policy/managed-mcp.json"),
                                    b"{}",
                                )
                                .unwrap(),
                                "windows" => {
                                    let directory = root.join("c71-system-policy/windows-policy");
                                    std::fs::create_dir(&directory).unwrap();
                                    std::fs::write(
                                        directory.join("managed-settings.json"),
                                        br#"{"wslInheritsWindowsSettings":true}"#,
                                    )
                                    .unwrap();
                                }
                                "backend" => {
                                    let file = root.join("reviewer-backends.json");
                                    let mut settings: Value =
                                        serde_json::from_slice(&std::fs::read(&file).unwrap())
                                            .unwrap();
                                    settings["claudeSubscription"]["profile"] =
                                        "/different/profile".into();
                                    std::fs::write(file, settings.to_string()).unwrap();
                                }
                                "revoke" => {
                                    let store = AuditStore::open(
                                        AbsolutePathBuf::from_absolute_path(root).unwrap(),
                                    )
                                    .await
                                    .unwrap();
                                    sqlx::query(
                                        "UPDATE audit_runs SET stop_generation=stop_generation+1",
                                    )
                                    .execute(&store.pool_for_tests())
                                    .await
                                    .unwrap();
                                }
                                _ => unreachable!(),
                            }
                        }
                    })
                })),
                ..Default::default()
            };
            let (controller, threads) = fixture_with_hooks(
                &temp,
                &server.uri(),
                CodexAuth::from_api_key("dummy"),
                false,
                Arc::new(|_, _| Box::pin(async { Ok(()) })),
                hooks,
            )
            .await;
            let id = seed_external(&controller, &temp, mode).await?;
            controller.drive(&id).await?;
            let store = controller.store().await?;
            let run = store.load_run(&id).await?;
            let launch = store.client_launch(&run.operation_id).await?.unwrap();
            assert_eq!(
                std::fs::read_to_string(temp.path().join("policy-control-count"))?,
                "5"
            );
            assert_eq!(
                store.submission_boundary(&run.operation_id).await?,
                SubmissionBoundary::Unsubmitted,
                "{mode}"
            );
            assert!(launch.prompt_write_started_at_ms.is_none());
            gone(launch.pid.unwrap() as u32).await?;
            assert!(
                !Path::new(&launch.mcp_config_path)
                    .parent()
                    .unwrap()
                    .join("receipt.json")
                    .exists()
            );
            threads
                .shutdown_all_threads_bounded(Duration::from_secs(5))
                .await;
        }
        Ok(())
    })
}

#[test]
fn missing_or_wrong_init_billing_is_post_submission_uncertainty() -> anyhow::Result<()> {
    run_test_with_large_stack("c71-billing", || async {
        for mode in ["wrong_billing", "missing_billing"] {
            let temp = tempfile::tempdir()?;
            let server = responses::start_mock_server().await;
            let (controller, threads) = real_fixture(&temp, &server.uri()).await;
            let id = seed_stream(&controller, &temp, mode, false).await?;
            let run = settled(&controller, &id).await?;
            let store = controller.store().await?;
            assert_eq!(
                store.submission_boundary(&run.operation_id).await?,
                SubmissionBoundary::SubmissionStarted
            );
            // C6's capture-failure path records Failed; its observation path can
            // record Unknown. Neither changes the durable submission boundary.
            assert!(matches!(
                run.assignment_state,
                AssignmentState::Failed | AssignmentState::Unknown
            ));
            assert!(
                store
                    .client_terminal_result(&run.operation_id)
                    .await?
                    .is_none()
            );
            let launch = store.client_launch(&run.operation_id).await?.unwrap();
            assert!(launch.prompt_write_started_at_ms.is_some());
            assert!(launch.client_accepted_at_ms.is_none());
            assert_eq!(
                store
                    .client_capture_failure(&run.assignment_id)
                    .await?
                    .as_deref(),
                Some("external_billing_source_mismatch_after_submission_charge_unknown")
            );
            assert_eq!(
                receipt(temp.path(), "stream-prompt-receipt.json").await?["count"],
                1
            );
            assert_no_private_files(controller.root.as_path())?;
            threads
                .shutdown_all_threads_bounded(Duration::from_secs(5))
                .await;
        }
        Ok(())
    })
}

#[test]
fn hostile_parent_environment_is_not_forwarded() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let home = temp.path().join("home");
    std::fs::create_dir(&home)?;
    let mut child = std::process::Command::new(std::env::current_exe()?);
    child.args(["--exact", "audex::controller::tests::execution::external::owned_launch_delivers_once_and_uses_real_bridge_with_revocation", "--nocapture"])
        .env("HOME", &home)
        .env("PATH", "/usr/bin:/bin")
        .env("CLAUDE_CONFIG_DIR", temp.path().join("hostile-profile"));
    for key in [
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_AUTH_TOKEN",
        "ANTHROPIC_BASE_URL",
        "CLAUDE_CODE_OAUTH_TOKEN",
        "CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR",
        "CLAUDE_CODE_API_KEY_HELPER_TTL_MS",
        "CLAUDE_CODE_USE_BEDROCK",
        "CLAUDE_CODE_USE_VERTEX",
        "CLAUDE_CODE_USE_FOUNDRY",
        "CLAUDE_CODE_USE_GATEWAY",
        "CLAUDE_CODE_REMOTE_SETTINGS_PATH",
        "CLAUDE_CODE_SETTINGS",
        "CLAUDE_CODE_GB_DISK_CACHE_WHEN_TELEMETRY_OFF",
        "CLAUDE_INTERNAL_FC_OVERRIDES",
        "CLAUDE_CODE_SESSION_KIND",
        "CLAUDE_BG_DISPATCHER_SUBSCRIPTION_TYPE",
        "CLAUDE_CODE_SUBSCRIPTION_TYPE",
        "CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST",
        "CLAUDE_CODE_SESSION_ID",
        "CLAUDE_CODE_ENTRYPOINT",
        "AWS_ACCESS_KEY_ID",
        "AWS_SECRET_ACCESS_KEY",
        "AWS_SESSION_TOKEN",
        "AWS_PROFILE",
        "AWS_SHARED_CREDENTIALS_FILE",
        "GOOGLE_APPLICATION_CREDENTIALS",
        "CLOUD_ML_REGION",
        "ANTHROPIC_FOUNDRY_API_KEY",
        "ANTHROPIC_FOUNDRY_BASE_URL",
        "AZURE_CLIENT_SECRET",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "NO_PROXY",
        "SSL_CERT_FILE",
        "NODE_EXTRA_CA_CERTS",
        "NODE_OPTIONS",
        "PYTHONPATH",
        "BASH_ENV",
        "OPENAI_API_KEY",
        "OPENAI_BASE_URL",
    ] {
        child.env(key, "dummy-c71-hostile-inherited-value");
    }
    let output = child.output()?;
    assert!(
        output.status.success(),
        "hostile environment fixture failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
    Ok(())
}

#[test]
fn executable_and_retained_policy_identity_cannot_be_substituted() -> anyhow::Result<()> {
    run_test_with_large_stack("c71-identity", || async {
        for mode in [
            "resolved_executable",
            "configured_version",
            "tool_binding",
            "retry_binding",
        ] {
            let temp = tempfile::tempdir()?;
            let server = responses::start_mock_server().await;
            let (controller, threads) = real_fixture(&temp, &server.uri()).await;
            let id = seed_external(&controller, &temp, mode).await?;
            let store = controller.store().await?;
            let run = store.load_run(&id).await?;
            let mut input =
                RetainedInput::decode(&store.attempt_input(&run.operation_id).await?.0)?;
            let path = controller.root.join("reviewer-backends.json");
            let mut settings: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
            match mode {
                "resolved_executable" => {
                    let link = temp.path().join("client-link");
                    std::os::unix::fs::symlink(temp.path().join("fake-client"), &link)?;
                    settings["claudeSubscription"]["executable"] = json!(link);
                }
                "configured_version" => {
                    settings["claudeSubscription"]["clientVersion"] = "2.1.999".into()
                }
                "tool_binding" => input
                    .bindings
                    .as_mut()
                    .unwrap()
                    .tool_exposure
                    .push("SendMessage".into()),
                "retry_binding" => input.bindings.as_mut().unwrap().limits.request_retries = 1,
                _ => unreachable!(),
            }
            let settings: BackendSettings = serde_json::from_value(settings)?;
            std::fs::write(path, serde_json::to_vec(&settings)?)?;
            input
                .bindings
                .as_mut()
                .unwrap()
                .backend_configuration_identity = settings.identity(&input.route())?;
            let raw = serde_json::to_string(&input)?;
            let digest = sha256_hex(raw.as_bytes());
            sqlx::query("UPDATE audit_attempt_inputs SET request_json=?,input_digest=? WHERE assignment_id=?")
                .bind(raw).bind(&digest).bind(&run.assignment_id).execute(&store.pool_for_tests()).await?;
            sqlx::query("UPDATE operations SET input_digest=? WHERE id=?")
                .bind(digest)
                .bind(&run.operation_id)
                .execute(&store.pool_for_tests())
                .await?;
            controller.drive(&id).await?;
            assert_eq!(
                store.submission_boundary(&run.operation_id).await?,
                SubmissionBoundary::Unsubmitted,
                "{mode}"
            );
            assert!(
                store.client_launch(&run.operation_id).await?.is_none(),
                "{mode}"
            );
            assert!(!temp.path().join("policy-control-count").exists());
            threads
                .shutdown_all_threads_bounded(Duration::from_secs(5))
                .await;
        }
        Ok(())
    })
}

#[test]
fn compatible_wsl_policy_passes_existing_pre_prompt_and_capture_path() -> anyhow::Result<()> {
    run_test_with_large_stack("c71-wsl-launch", || async {
        for mode in [
            "ordinary",
            "disabled",
            "flag_only",
            "windows_file",
            "windows_registry",
            "wrong_handshake_source",
        ] {
            let temp = tempfile::tempdir()?;
            let server = responses::start_mock_server().await;
            let (controller, threads) = real_fixture(&temp, &server.uri()).await;
            let id = seed_stream(&controller, &temp, "valid", false).await?;
            let machine = controller.root.join("c71-system-policy");
            let directory = machine.join("windows-policy");
            std::fs::create_dir(&directory)?;
            let active = json!({"wslInheritsWindowsSettings":true,"forceRemoteSettingsRefresh":true,"disableAllHooks":true});
            let registry_value = match mode {
                "disabled" => Some(
                    json!({"wslInheritsWindowsSettings":false,"env":{"ANTHROPIC_API_KEY":"dummy"}}),
                ),
                "flag_only" => Some(json!({"wslInheritsWindowsSettings":true})),
                "windows_registry" | "wrong_handshake_source" => Some(active.clone()),
                _ => None,
            };
            let (stdout, stderr, code) = match registry_value {
                Some(value) => (
                    format!(
                        "HKEY_LOCAL_MACHINE\\SOFTWARE\\Policies\\ClaudeCode\n    Settings    REG_SZ    {value}\n"
                    ),
                    "",
                    0,
                ),
                None => (
                    String::new(),
                    "ERROR: The system was unable to find the specified registry key or value.\n",
                    1,
                ),
            };
            let registry = machine.join("windows-registry");
            std::fs::write(
                &registry,
                format!(
                    "#!/usr/bin/python3\nimport sys\nassert sys.argv[1:] == ['query', r'HKLM\\SOFTWARE\\Policies\\ClaudeCode', '/v', 'Settings']\nsys.stdout.write({})\nsys.stderr.write({})\nsys.exit({code})\n",
                    serde_json::to_string(&stdout)?,
                    serde_json::to_string(stderr)?
                ),
            )?;
            std::fs::set_permissions(registry, std::fs::Permissions::from_mode(0o700))?;
            let inherited = matches!(
                mode,
                "windows_file" | "windows_registry" | "wrong_handshake_source"
            );
            if mode == "windows_file" {
                std::fs::write(directory.join("managed-settings.json"), active.to_string())?;
            }
            if inherited {
                // The accepted launch must use the selected Windows admin
                // policy. Replies are independent of preflight reads.
                std::fs::remove_file(machine.join("managed-settings.json"))?;
                std::fs::write(
                    temp.path().join("policy-scenario.json"),
                    json!({
                        "managed_settings": active,
                        "setting_source": if mode == "windows_registry" {
                            "User settings, Enterprise managed settings (HKLM)"
                        } else { "User settings, Enterprise managed settings (file)" }
                    })
                    .to_string(),
                )?;
            }
            let run = settled(&controller, &id).await?;
            let store = controller.store().await?;
            let launch = store.client_launch(&run.operation_id).await?.unwrap();
            assert_eq!(
                std::fs::read_to_string(temp.path().join("policy-control-count"))?,
                "5",
                "{mode}"
            );
            if mode == "wrong_handshake_source" {
                assert_eq!(run.assignment_state, AssignmentState::Failed);
                assert_eq!(
                    store.submission_boundary(&run.operation_id).await?,
                    SubmissionBoundary::Unsubmitted
                );
                assert!(launch.prompt_write_started_at_ms.is_none());
            } else {
                assert_eq!(run.assignment_state, AssignmentState::Succeeded, "{mode}");
                assert!(launch.prompt_write_started_at_ms.is_some(), "{mode}");
            }
            gone(launch.pid.unwrap() as u32).await?;
            threads
                .shutdown_all_threads_bounded(Duration::from_secs(5))
                .await;
        }
        Ok(())
    })
}
