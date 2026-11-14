//! G6: production controller/store/routing with deterministic children and real stdio MCP.
use super::*;
mod capture;
mod control_boundaries;
mod guidance_parity;
mod launch_qualification;
pub(super) mod lifecycle_qualification;
mod limits;
mod native_peer;
mod official_client;
mod publication_races;
mod repairs;
mod restart_qualification;
mod socket_qualification;
mod source_tools;
mod stop_storage;
mod stream_qualification;
mod subscription_policy;
use crate::audex::backends::BackendSettings;
use crate::audex::retained::RetainedInput;
use crate::audex::snapshot::manifest::sha256_hex;
use crate::audex::store::SubmissionBoundary;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

async fn seed_external(
    controller: &AuditController,
    temp: &tempfile::TempDir,
    key: &str,
) -> anyhow::Result<String> {
    let id = start(controller, key).await;
    retain_external(controller, temp, &id).await?;
    Ok(id)
}

async fn retain_external(
    controller: &AuditController,
    temp: &tempfile::TempDir,
    id: &str,
) -> anyhow::Result<()> {
    let executable = temp.path().join("fake-client");
    std::fs::write(
        &executable,
        include_str!("../../../external/fake_client.py"),
    )?;
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700))?;
    // Explicit fixture provisioning: keep the configured root short enough for
    // the official client's full UUID/tmp/cc-socks/PID.sock suffix. The test runner
    // owns and removes this root after all crash/reconstruction fixtures finish.
    let runtime_link = temp.path().join("client-runtime");
    let runtime = if runtime_link.try_exists()? {
        runtime_link.canonicalize()?
    } else {
        let runtime = tempfile::Builder::new()
            .prefix("")
            .rand_bytes(6)
            .tempdir_in(std::env::var("AUDEX_CLIENT_TEST_RUNTIME_ROOT")?)?
            .keep();
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700))?;
        std::os::unix::fs::symlink(&runtime, &runtime_link)?;
        runtime
    };
    let profile = temp.path().join("subscription-profile");
    std::fs::create_dir_all(&profile)?;
    std::fs::set_permissions(&profile, std::fs::Permissions::from_mode(0o700))?;
    std::fs::write(
        profile.join("settings.json"),
        json!({
            "disableAllHooks":true,"disableClaudeAiConnectors":true,"syncClaudeAiPlugins":false,
            "syncClaudeAiSkills":false,"autoMemoryEnabled":false,"crossSessionInbound":"refuse",
            "permissions":{"deny":["SendMessage","ListAgents"]},"theme":"dark"
        })
        .to_string(),
    )?;
    std::fs::create_dir_all(controller.root.join("c71-system-policy"))?;
    std::fs::write(
        controller
            .root
            .join("c71-system-policy/managed-settings.json"),
        r#"{"forceRemoteSettingsRefresh":true}"#,
    )?;
    std::fs::write(
        temp.path().join("fake_policy.py"),
        include_str!("../../../external/fake_policy.py"),
    )?;
    let settings: BackendSettings = serde_json::from_value(
        json!({"version":1,"claudeSubscription":{
            "executable":executable,"clientVersion":"2.1.314","profile":profile,"runtimeRoot":runtime
        }}),
    )?;
    std::fs::write(
        controller.root.join("reviewer-backends.json"),
        serde_json::to_vec(&settings)?,
    )?;
    let store = controller.store().await?;
    let run = store.load_run(&id).await?;
    let (raw, _, _, _) = store.attempt_input(&run.operation_id).await?;
    let mut input = RetainedInput::decode(&raw)?;
    input.provider = "anthropic".into();
    input.model = "claude-haiku-5-5".into();
    input.resolved_model = Some(input.model.clone());
    input.expected_served_model = Some(input.model.clone());
    input.reasoning_effort = None;
    input.execution_adapter = Some(AuditExecutionAdapter::ClaudeClient);
    input.authentication_route = Some(AuditAuthenticationRoute::ClaudeSubscription);
    let route = input.route();
    let bindings = input.bindings.as_mut().unwrap();
    bindings.backend_configuration_identity = settings.identity(&route)?;
    bindings.native_configuration_identity = None;
    bindings.tool_exposure = ["list", "read", "search"]
        .map(|name| format!("mcp__audex_source__{name}"))
        .to_vec();
    let raw = serde_json::to_string(&input)?;
    let digest = sha256_hex(raw.as_bytes());
    let mut tx = store.pool_for_tests().begin().await?;
    sqlx::query(
        "UPDATE audit_attempt_inputs SET request_json=?,input_digest=? WHERE assignment_id=?",
    )
    .bind(&raw)
    .bind(&digest)
    .bind(&run.assignment_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE operations SET input_digest=? WHERE id=?")
        .bind(&digest)
        .bind(&run.operation_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

async fn receipt(directory: &Path, name: &str) -> anyhow::Result<Value> {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Ok(raw) = std::fs::read(directory.join(name))
                && let Ok(value) = serde_json::from_slice(&raw)
            {
                return Ok(value);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?
}

async fn launch_directory(controller: &AuditController, id: &str) -> anyhow::Result<PathBuf> {
    let store = controller.store().await?;
    let run = store.load_run(id).await?;
    let launch = store
        .client_launch(&run.operation_id)
        .await?
        .expect("durable launch");
    Ok(PathBuf::from(launch.mcp_config_path)
        .parent()
        .unwrap()
        .to_owned())
}

async fn gone(pid: u32) -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(5), async {
        while Path::new(&format!("/proc/{pid}")).exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    Ok(())
}

#[test]
fn owned_launch_delivers_once_and_uses_real_bridge_with_revocation() -> anyhow::Result<()> {
    run_test_with_large_stack("c61-owned-launch", || async {
        let temp = tempfile::tempdir()?;
        let server = responses::start_mock_server().await;
        let (controller, threads) = real_fixture(&temp, &server.uri()).await;
        let id = seed_external(&controller, &temp, "owned").await?;
        controller.drive(&id).await?;
        let directory = launch_directory(&controller, &id).await?;
        let observed = receipt(&directory, "receipt.json").await?;
        let store = controller.store().await?;
        let run = store.load_run(&id).await?;
        let launch = store.client_launch(&run.operation_id).await?.unwrap();
        assert_eq!(launch.route, "claude_subscription");
        assert_eq!(launch.client_version, "2.1.314");
        assert_eq!(launch.configuration_identity.len(), 64);
        assert_eq!(launch.launch_configuration_digest.len(), 64);
        assert!(launch.intent_at_ms <= launch.identity_at_ms.unwrap());
        assert!(launch.identity_at_ms.unwrap() <= launch.prompt_write_started_at_ms.unwrap());
        assert_eq!(
            launch.child_identity().unwrap(),
            codex_app_server_daemon::read_linux_child_identity(
                observed["pid"].as_u64().unwrap() as u32,
                &launch.mcp_config_path
            )?
        );
        assert!(
            codex_app_server_daemon::read_linux_child_identity(
                observed["pid"].as_u64().unwrap() as u32,
                "foreign-nonce"
            )
            .is_err()
        );
        let config: Value = serde_json::from_slice(&std::fs::read(&launch.mcp_config_path)?)?;
        let mcp = &config["mcpServers"]["audex_source"];
        let arguments: Vec<String> = serde_json::from_value(mcp["args"].clone())?;
        let binding_index = arguments.iter().position(|arg| arg == "--binding").unwrap() + 1;
        let authority = crate::audex::store::source::ReadOnlySource::open(
            &controller.root,
            serde_json::from_str(&arguments[binding_index])?,
            run.snapshot_id.clone(),
            store.attempt_input(&run.operation_id).await?.3,
        )
        .await?;
        authority
            .verify_launch_parent(launch.pid.unwrap() as u32, true)
            .await?;
        for field in ["start_ticks", "boot_id"] {
            let query = match field {
                "start_ticks" => {
                    "UPDATE audit_client_launches SET start_ticks=start_ticks+1 WHERE operation_id=?"
                }
                _ => {
                    "UPDATE audit_client_launches SET boot_id='previous-boot' WHERE operation_id=?"
                }
            };
            sqlx::query(query)
                .bind(&run.operation_id)
                .execute(&store.pool_for_tests())
                .await?;
            assert!(
                authority
                    .verify_launch_parent(launch.pid.unwrap() as u32, true)
                    .await
                    .is_err()
            );
            sqlx::query(
                "UPDATE audit_client_launches SET start_ticks=?,boot_id=? WHERE operation_id=?",
            )
            .bind(launch.start_ticks)
            .bind(&launch.boot_id)
            .bind(&run.operation_id)
            .execute(&store.pool_for_tests())
            .await?;
        }
        for corruption in ["foreign_parent", "foreign_nonce"] {
            let mut args: Vec<String> = serde_json::from_value(mcp["args"].clone())?;
            if corruption == "foreign_nonce" {
                let index = args.iter().position(|a| a == "--binding").unwrap() + 1;
                let mut binding: Value = serde_json::from_str(&args[index])?;
                binding["execution_id"] = uuid::Uuid::now_v7().to_string().into();
                args[index] = binding.to_string();
            }
            let output = tokio::time::timeout(
                Duration::from_secs(5),
                tokio::process::Command::new(mcp["command"].as_str().unwrap())
                    .args(args)
                    .env_clear()
                    .env("PATH", "/usr/bin:/bin")
                    .env("CODEX_HOME", directory.join("bridge-home"))
                    .stdin(std::process::Stdio::null())
                    .kill_on_drop(true)
                    .output(),
            )
            .await??;
            assert!(!output.status.success(), "{corruption}");
            assert!(
                output.stdout.is_empty(),
                "untrusted bridge produced MCP output"
            );
        }
        assert_eq!(observed["prompt_count"], 1);
        assert_eq!(observed["parent_death_signal"], 9);
        assert_eq!(
            store.submission_boundary(&run.operation_id).await?,
            SubmissionBoundary::SubmissionStarted
        );
        assert!(
            store
                .execution(&run.operation_id)
                .await?
                .unwrap()
                .1
                .is_none(),
            "pipe write is not acceptance"
        );
        for _ in 0..3 {
            controller.drive(&id).await?;
        }
        assert_eq!(
            std::fs::read_to_string(directory.join("spawn-count"))?,
            "spawn\n"
        );
        assert!(
            threads.list_thread_ids().await.is_empty(),
            "external launch creates no native thread"
        );
        sqlx::query("UPDATE operations SET lease_expires_at_ms=0 WHERE id=?")
            .bind(&run.operation_id)
            .execute(&store.pool_for_tests())
            .await?;
        std::fs::write(directory.join("read-again"), b"")?;
        let refused = receipt(&directory, "revoked.json").await?;
        assert_eq!(refused["result"]["isError"], true);
        println!(
            "C61 real MCP receipt: {}",
            json!({"nonce":observed["nonce"],"prompt_count":observed["prompt_count"],"parent_death_signal":observed["parent_death_signal"],"transcript":observed["transcript"],"revoked":refused})
        );
        std::fs::write(directory.join("stop"), b"")?;
        gone(observed["pid"].as_u64().unwrap() as u32).await?;
        gone(observed["bridge_pid"].as_u64().unwrap() as u32).await?;
        for _ in 0..3 {
            controller.drive(&id).await?;
        }
        assert_eq!(
            std::fs::read_to_string(directory.join("spawn-count"))?,
            "spawn\n"
        );
        Ok(())
    })
}

#[test]
fn crash_boundaries_preserve_original_and_allow_one_stopped_empty_replacement() -> anyhow::Result<()>
{
    run_test_with_large_stack("c61-crash-windows", || async {
        for stage in [
            "launch_intent",
            "child_identity",
            "prompt_write_started",
            "prompt_written",
        ] {
            let temp = tempfile::tempdir()?;
            let server = responses::start_mock_server().await;
            let entered = Arc::new(Notify::new());
            let (signal, never) = (entered.clone(), Arc::new(Notify::new()));
            let hooks = crate::audex::dispatch::DispatchTestHooks {
                checkpoint: Some(Arc::new(move |name| {
                    let (signal, never) = (signal.clone(), never.clone());
                    Box::pin(async move {
                        if name == stage {
                            signal.notify_one();
                            never.notified().await;
                        }
                    })
                })),
                ..Default::default()
            };
            let (controller, _) = fixture_with_hooks(
                &temp,
                &server.uri(),
                CodexAuth::from_api_key("dummy"),
                false,
                Arc::new(|_, _| Box::pin(async { Ok(()) })),
                hooks,
            )
            .await;
            let id = seed_external(&controller, &temp, stage).await?;
            let (driver, run_id) = (controller.clone(), id.clone());
            let task = tokio::spawn(async move { driver.drive(&run_id).await });
            tokio::time::timeout(Duration::from_secs(10), entered.notified()).await?;
            let store = controller.store().await?.clone();
            let run = store.load_run(&id).await?;
            let launch = store.client_launch(&run.operation_id).await?.unwrap();
            assert_eq!(launch.pid.is_some(), stage != "launch_intent");
            assert_eq!(
                launch.prompt_write_started_at_ms.is_some(),
                matches!(stage, "prompt_write_started" | "prompt_written")
            );
            let directory = PathBuf::from(&launch.mcp_config_path)
                .parent()
                .unwrap()
                .to_owned();
            if stage == "prompt_written" {
                assert_eq!(
                    receipt(&directory, "receipt.json").await?["prompt_count"],
                    1
                );
            } else if stage != "launch_intent" {
                tokio::time::timeout(Duration::from_secs(5), async {
                    while !directory.join("spawn-count").exists() {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                })
                .await?;
            }
            task.abort();
            let _ = task.await;
            // Dropping the dispatcher removes only its in-memory owned handles.
            drop(controller);
            if let Some(pid) = launch.pid {
                gone(pid as u32).await?;
            }
            sqlx::query("UPDATE operations SET lease_expires_at_ms=0 WHERE id=?")
                .bind(&run.operation_id)
                .execute(&store.pool_for_tests())
                .await?;
            let (restarted, _) = real_fixture(&temp, &server.uri()).await;
            for _ in 0..5 {
                restarted.tick().await?;
            }
            let after = store.client_launch(&run.operation_id).await?.unwrap();
            assert_eq!(after.execution_id, launch.execution_id);
            assert_eq!(
                after.prompt_write_started_at_ms,
                launch.prompt_write_started_at_ms
            );
            let directory = PathBuf::from(&after.mcp_config_path)
                .parent()
                .unwrap()
                .to_owned();
            let count = std::fs::read_to_string(directory.join("spawn-count"))
                .unwrap_or_default()
                .lines()
                .count();
            assert_eq!(count, usize::from(stage != "launch_intent"));
            let attempts = store.attempts(&id).await?;
            assert_eq!(attempts.len(), 2, "{stage}: one stopped, empty replacement");
            assert_eq!(store.list_run_ids("", 100).await?.len(), 1);
            let replacement = attempts.iter().find(|a| a.current).unwrap();
            assert_ne!(replacement.assignment_id, run.assignment_id);
            assert_eq!(replacement.snapshot_id, run.snapshot_id);
            assert_eq!(replacement.attempt_ordinal, 2);
            assert_eq!(
                store.attempt_input(&replacement.operation_id).await?,
                store.attempt_input(&run.operation_id).await?
            );
            assert!(!directory.join("receipt.json").exists() || stage == "prompt_written");
            println!(
                "M5 crash {stage}: original nonce={}, launches={count}, prompt boundary={:?}; original not replayed, one same-audit replacement",
                after.execution_id, after.prompt_write_started_at_ms
            );
            restarted.abort_audit(&id).await?;
            restarted.dispatch.reconcile_resources().await?;
        }
        Ok(())
    })
}

#[test]
fn configuration_and_spawn_failures_preserve_healthy_native_execution() -> anyhow::Result<()> {
    run_test_with_large_stack("c61-failure-native-peer", || async {
        for failure in ["configuration", "version", "spawn"] {
            let temp = tempfile::tempdir()?;
            let server = responses::start_mock_server().await;
            let (controller, threads) = real_fixture(&temp, &server.uri()).await;
            let id = seed_external(&controller, &temp, failure).await?;
            if failure == "configuration" {
                let path = controller.root.join("reviewer-backends.json");
                let mut settings: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
                settings["claudeSubscription"]["clientVersion"] = "unqualified".into();
                std::fs::write(path, serde_json::to_vec(&settings)?)?;
            } else if failure == "version" {
                std::fs::write(
                    temp.path().join("fake-client"),
                    "#!/usr/bin/python3\nprint('999.0.0 (Claude Code)')\n",
                )?;
            } else {
                std::fs::write(temp.path().join("fake-client"), "#!/absent/interpreter\n")?;
            }
            controller.drive(&id).await?;
            let store = controller.store().await?;
            let failed = store.load_run(&id).await?;
            assert_eq!(failed.assignment_state, AssignmentState::Failed);
            assert_eq!(
                store.submission_boundary(&failed.operation_id).await?,
                SubmissionBoundary::Unsubmitted
            );
            let launch = store.client_launch(&failed.operation_id).await?;
            assert_eq!(launch.is_some(), failure != "configuration");
            assert!(
                launch
                    .as_ref()
                    .is_none_or(|l| l.pid.is_none() && l.prompt_write_started_at_ms.is_none())
            );
            // A real native executor, real local Responses transport and normal result validation.
            let native = start(&controller, &format!("native-{failure}")).await;
            let run = store.load_run(&native).await?;
            let raw = json!({"schema_version":1,"snapshot_id":run.snapshot_id,"assignment_id":run.assignment_id,
                "summary":"Healthy native peer","findings":[]}).to_string();
            let mock = responses::mount_sse_sequence(
                &server,
                vec![sse(vec![
                    ev_response_created("native"),
                    ev_assistant_message("answer", &raw),
                    ev_completed("native"),
                ])],
            )
            .await;
            assert_eq!(finished(&controller, &native).await["phase"], "completed");
            assert_eq!(mock.requests().len(), 1);
            for _ in 0..3 {
                controller.drive(&id).await?;
            }
            threads
                .shutdown_all_threads_bounded(Duration::from_secs(5))
                .await;
            println!("C61 {failure}: external unsubmitted, native completed once");
        }
        Ok(())
    })
}

#[test]
fn isolated_owner_process() -> anyhow::Result<()> {
    // This entry runs only inside the owned supervisor fixture. A normal G2
    // invocation has no external process configuration and returns immediately.
    let Some(root) = std::env::var_os("AUDEX_C61_OWNER_ROOT") else {
        return Ok(());
    };
    run_test_with_large_stack("c61-process-owner", || async move {
        let root = PathBuf::from(root);
        let stage = std::env::var("AUDEX_C61_OWNER_STAGE")?;
        let temp = tempfile::Builder::new()
            .prefix("owner-")
            .tempdir_in(&root)?;
        let server = responses::start_mock_server().await;
        let checkpoint_root = root.clone();
        let checkpoint_home = temp.path().to_owned();
        let hooks = crate::audex::dispatch::DispatchTestHooks {
            checkpoint: Some(Arc::new(move |name| {
                let (root, home, stage) = (
                    checkpoint_root.clone(),
                    checkpoint_home.clone(),
                    stage.clone(),
                );
                Box::pin(async move {
                    if name == stage {
                        let store = AuditStore::open(
                            AbsolutePathBuf::from_absolute_path(home.join("audex")).unwrap(),
                        )
                        .await
                        .unwrap();
                        let run_id = store.list_run_ids("", 10).await.unwrap().pop().unwrap();
                        let run = store.load_run(&run_id).await.unwrap();
                        let launch = store
                            .client_launch(&run.operation_id)
                            .await
                            .unwrap()
                            .unwrap();
                        if name == "prompt_written" {
                            receipt(
                                Path::new(&launch.mcp_config_path).parent().unwrap(),
                                "receipt.json",
                            )
                            .await
                            .unwrap();
                        }
                        std::fs::write(root.join("owner-checkpoint.json"), serde_json::to_vec(&json!({
                        "home":home,"run_id":run_id,"pid":launch.pid,"nonce":launch.execution_id,"prompt_write_started":launch.prompt_write_started_at_ms
                    })).unwrap()).unwrap();
                        std::future::pending::<()>().await;
                    }
                })
            })),
            ..Default::default()
        };
        let (controller, _) = fixture_with_hooks(
            &temp,
            &server.uri(),
            CodexAuth::from_api_key("dummy"),
            false,
            Arc::new(|_, _| Box::pin(async { Ok(()) })),
            hooks,
        )
        .await;
        let id = seed_external(&controller, &temp, "parent-death").await?;
        controller.drive(&id).await?;
        anyhow::bail!("owner did not reach checkpoint");
    })
}

#[test]
fn daemon_death_kills_owned_children_without_replay() -> anyhow::Result<()> {
    run_test_with_large_stack("c61-parent-death", || async {
        for stage in [
            "launch_intent",
            "child_identity",
            "prompt_write_started",
            "prompt_written",
        ] {
            let root = tempfile::tempdir()?;
            let output = tokio::process::Command::new("python3")
                .arg("-c")
                .arg(include_str!("../../../external/parent_death.py"))
                .arg(std::env::current_exe()?)
                .arg(root.path())
                .arg(stage)
                .kill_on_drop(true)
                .output()
                .await?;
            anyhow::ensure!(
                output.status.success(),
                "parent death {stage}: {} {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            println!(
                "C61 daemon death: {}",
                String::from_utf8_lossy(&output.stdout)
            );
            let record: Value =
                serde_json::from_slice(&std::fs::read(root.path().join("owner-checkpoint.json"))?)?;
            let home = PathBuf::from(record["home"].as_str().unwrap());
            let store =
                AuditStore::open(AbsolutePathBuf::from_absolute_path(home.join("audex"))?).await?;
            let run = store.load_run(record["run_id"].as_str().unwrap()).await?;
            let launch = store.client_launch(&run.operation_id).await?.unwrap();
            assert_eq!(launch.execution_id, record["nonce"].as_str().unwrap());
            assert_eq!(
                launch.prompt_write_started_at_ms.is_some(),
                matches!(stage, "prompt_write_started" | "prompt_written")
            );
            assert_eq!(store.execution(&run.operation_id).await?.unwrap().1, None);
            // Read-only snapshot directories are owned by this fixture too.
            fn writable(path: &Path) {
                if path.is_dir() {
                    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
                    for entry in std::fs::read_dir(path).unwrap().flatten() {
                        writable(&entry.path());
                    }
                }
            }
            writable(root.path());
        }
        Ok(())
    })
}

#[test]
fn changed_private_configuration_or_revoked_authority_prevents_prompt() -> anyhow::Result<()> {
    run_test_with_large_stack("c61-preprompt-fencing", || async {
        for change in [
            "mcp_before_spawn",
            "mcp_before_prompt",
            "revoke_before_prompt",
        ] {
            let temp = tempfile::tempdir()?;
            let server = responses::start_mock_server().await;
            let root = temp.path().join("audex");
            let hooks = crate::audex::dispatch::DispatchTestHooks {
                checkpoint: Some(Arc::new(move |stage| {
                    let root = root.clone();
                    Box::pin(async move {
                        if (change == "mcp_before_spawn" && stage == "launch_intent")
                            || (change != "mcp_before_spawn" && stage == "child_identity")
                        {
                            let store = AuditStore::open(
                                AbsolutePathBuf::from_absolute_path(root).unwrap(),
                            )
                            .await
                            .unwrap();
                            let id = store.list_run_ids("", 10).await.unwrap().pop().unwrap();
                            let run = store.load_run(&id).await.unwrap();
                            let launch = store
                                .client_launch(&run.operation_id)
                                .await
                                .unwrap()
                                .unwrap();
                            if change == "revoke_before_prompt" {
                                sqlx::query("UPDATE audit_runs SET stop_generation=stop_generation+1 WHERE id=?")
                                    .bind(&id).execute(&store.pool_for_tests()).await.unwrap();
                            } else {
                                std::fs::write(launch.mcp_config_path, b"{}").unwrap();
                            }
                        }
                    })
                })),
                ..Default::default()
            };
            let (controller, _) = fixture_with_hooks(
                &temp,
                &server.uri(),
                CodexAuth::from_api_key("dummy"),
                false,
                Arc::new(|_, _| Box::pin(async { Ok(()) })),
                hooks,
            )
            .await;
            let id = seed_external(&controller, &temp, change).await?;
            controller.drive(&id).await?;
            let store = controller.store().await?;
            let run = store.load_run(&id).await?;
            let launch = store.client_launch(&run.operation_id).await?.unwrap();
            assert_eq!(
                store.submission_boundary(&run.operation_id).await?,
                SubmissionBoundary::Unsubmitted
            );
            assert!(launch.prompt_write_started_at_ms.is_none());
            if let Some(pid) = launch.pid {
                gone(pid as u32).await?;
            }
            let directory = Path::new(&launch.mcp_config_path).parent().unwrap();
            assert!(!directory.join("receipt.json").exists());
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
            println!("C61 {change}: refused before prompt, no replay");
        }
        Ok(())
    })
}
