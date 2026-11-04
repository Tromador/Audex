//! D1: reject fallback-inducing roots and retain escaped socket obligations.
use super::*;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;

async fn use_runtime(controller: &AuditController, id: &str, runtime: &Path) -> anyhow::Result<()> {
    let path = controller.root.join("reviewer-backends.json");
    let mut settings: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
    settings["claudeSubscription"]["runtimeRoot"] = runtime.display().to_string().into();
    std::fs::write(path, serde_json::to_vec(&settings)?)?;
    let settings: BackendSettings = serde_json::from_value(settings)?;
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
    sqlx::query(
        "UPDATE audit_attempt_inputs SET request_json=?,input_digest=? WHERE assignment_id=?",
    )
    .bind(raw)
    .bind(&digest)
    .bind(&run.assignment_id)
    .execute(&store.pool_for_tests())
    .await?;
    sqlx::query("UPDATE operations SET input_digest=? WHERE id=?")
        .bind(digest)
        .bind(&run.operation_id)
        .execute(&store.pool_for_tests())
        .await?;
    Ok(())
}

#[test]
fn unsafe_socket_path_lengths_are_rejected_before_any_spawn_or_prompt() -> anyhow::Result<()> {
    run_test_with_large_stack("d1-path-length", || async {
        for unicode in [false, true] {
            let temp = tempfile::tempdir()?;
            let server = responses::start_mock_server().await;
            let (controller, threads) = real_fixture(&temp, &server.uri()).await;
            let id = capture::seed_stream(&controller, &temp, "valid", true).await?;
            let short = temp.path().join("client-runtime").canonicalize()?;
            let pid_max: u32 = std::fs::read_to_string("/proc/sys/kernel/pid_max")?
                .trim()
                .parse()?;
            let suffix =
                format!("/00000000-0000-0000-0000-000000000000/tmp/cc-socks/{pid_max}.sock");
            assert_eq!(short.as_os_str().as_bytes().len() + suffix.len(), 103);
            let unsafe_root = if unicode {
                let text = short.to_str().unwrap();
                PathBuf::from(format!("{}é", &text[..text.len() - 1]))
            } else {
                PathBuf::from(format!("{}x", short.display()))
            };
            assert_eq!(unsafe_root.as_os_str().as_bytes().len() + suffix.len(), 104);
            std::fs::create_dir(&unsafe_root)?;
            std::fs::set_permissions(&unsafe_root, std::fs::Permissions::from_mode(0o700))?;
            use_runtime(&controller, &id, &unsafe_root).await?;
            controller.drive(&id).await?;
            let store = controller.store().await?;
            let run = store.load_run(&id).await?;
            let launch = store.client_launch(&run.operation_id).await?;
            // Keep the pre-fix regression run safe even when the launch is wrongly allowed.
            controller
                .dispatch
                .request_interrupt(run.operation_id.clone())
                .await;
            if let Some(child) = launch.as_ref().and_then(|l| l.child_identity()) {
                gone(child.pid).await?;
            }
            assert!(launch.is_none(), "unsafe root reached the launch boundary");
            assert!(store.execution(&run.operation_id).await?.is_none());
            assert_eq!(
                store.submission_boundary(&run.operation_id).await?,
                SubmissionBoundary::Unsubmitted
            );
            assert!(!temp.path().join("stream-launches").exists());
            assert_eq!(
                std::fs::read_dir(&unsafe_root)?.count(),
                0,
                "must not relocate or prepare a runtime"
            );
            threads
                .shutdown_all_threads_bounded(Duration::from_secs(5))
                .await;
            println!(
                "D1 unsafe root: unicode={unicode}, maximum socket bytes=104; no launch identity, spawn, prompt or relocation"
            );
        }
        Ok(())
    })
}

#[test]
fn outside_socket_obligations_survive_stop_and_reconstruction_without_deletion()
-> anyhow::Result<()> {
    run_test_with_large_stack("d1-outside-socket", || async {
        for replacement in [false, true] {
            let temp = tempfile::tempdir()?;
            let server = responses::start_mock_server().await;
            let (controller, threads) = real_fixture(&temp, &server.uri()).await;
            let id = capture::seed_stream(&controller, &temp, "valid", true).await?;
            let outside = temp.path().join("outside.sock");
            std::fs::write(
                temp.path().join("stream-scenario.json"),
                json!({
                "mode":"valid","gate":true,"lifecycle":true,"socket_path":outside})
                .to_string(),
            )?;
            controller.drive(&id).await?;
            let ready = receipt(temp.path(), "lifecycle-ready.json")
                .await
                .map_err(|e| anyhow::anyhow!("outside socket fixture startup: {e}"))?;
            let foreign = if replacement {
                std::fs::remove_file(&outside)?;
                Some(std::os::unix::net::UnixListener::bind(&outside)?)
            } else {
                None
            };
            let before = std::fs::symlink_metadata(&outside)?;
            let store = controller.store().await?;
            let run = store.load_run(&id).await?;
            let launch = store.client_launch(&run.operation_id).await?.unwrap();
            let mut durable = Value::Null;
            for _ in 0..10 {
                controller.dispatch.reconcile_resources().await?;
                durable = serde_json::from_str(
                    &store.launch_lifecycle(&run.operation_id).await?.ownership,
                )?;
                if durable["unresolved_socket_paths"]
                    .as_array()
                    .is_some_and(|paths| paths.contains(&json!(outside)))
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            controller.abort_audit(&id).await?;
            for role in ["client", "bridge", "member"] {
                gone(ready[role].as_u64().unwrap() as u32).await?;
            }
            assert!(
                durable["unresolved_socket_paths"]
                    .as_array()
                    .is_some_and(|paths| paths.contains(&json!(outside))),
                "escaped socket observation was not durably retained: {durable}"
            );
            // New controller/store/dispatch reads the original durable obligation;
            // the dead client cannot rediscover it from /proc.
            let (reopened, reopened_threads) = real_fixture(&temp, &server.uri()).await;
            for _ in 0..3 {
                reopened.tick().await?;
            }
            let view = reopened
                .store()
                .await?
                .local_execution_view(&run.operation_id)
                .await?
                .unwrap();
            assert!(view.stop_confirmed_at_ms.is_some());
            assert!(view.cleanup_finished_at_ms.is_none());
            assert_eq!(view.status, "socket_cleanup_unconfirmed");
            let retained: Value =
                serde_json::from_str(&store.launch_lifecycle(&run.operation_id).await?.ownership)?;
            assert_eq!(
                retained["unresolved_socket_paths"],
                durable["unresolved_socket_paths"]
            );
            let after = std::fs::symlink_metadata(&outside)?;
            assert_eq!(
                (before.dev(), before.ino(), before.mode()),
                (after.dev(), after.ino(), after.mode())
            );
            assert_eq!(
                store
                    .client_launch(&run.operation_id)
                    .await?
                    .unwrap()
                    .execution_id,
                launch.execution_id
            );
            assert_eq!(
                std::fs::read_to_string(temp.path().join("stream-launches"))?
                    .lines()
                    .count(),
                1
            );
            drop(foreign);
            std::fs::remove_file(&outside)?;
            threads
                .shutdown_all_threads_bounded(Duration::from_secs(5))
                .await;
            reopened_threads
                .shutdown_all_threads_bounded(Duration::from_secs(5))
                .await;
            println!(
                "D1 outside socket: replacement={replacement}; observation persisted before stop and retained on reconstruction; cleanup unconfirmed; original inode preserved; one prompt"
            );
        }
        Ok(())
    })
}
