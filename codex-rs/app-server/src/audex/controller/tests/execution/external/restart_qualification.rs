//! Kill and reconstruct the real controller/store/dispatch in separate daemon processes.
use super::capture::{seed_stream, settled};
use super::*;

async fn checkpoint(root: &Path, home: &Path, stage: &str) -> anyhow::Result<()> {
    let store = AuditStore::open(AbsolutePathBuf::from_absolute_path(home.join("audex"))?).await?;
    let id = store.list_run_ids("", 10).await?.pop().unwrap();
    let run = store.load_run(&id).await?;
    let launch = store.client_launch(&run.operation_id).await?.unwrap();
    std::fs::write(root.join("checkpoint.json"), json!({"home":home,"run_id":id,"operation":run.operation_id,
        "execution":launch.execution_id,"stage":stage,"prompt_started":launch.prompt_write_started_at_ms,
        "report":read_report_identity(&store,&id).await?}).to_string())?;
    Ok(())
}

async fn read_report_identity(store: &AuditStore, id: &str) -> anyhow::Result<Value> {
    let report = store.load_run(id).await?.report;
    Ok(report.map(|r| json!({"id":r.id,"input_digest":r.input_digest,"path":r.relative_path,"content_digest":r.content_digest})).unwrap_or(Value::Null))
}

#[test]
fn isolated_lifecycle_daemon() -> anyhow::Result<()> {
    let Some(root) = std::env::var_os("AUDEX_C63_RESTART_ROOT") else {
        return Ok(());
    };
    run_test_with_large_stack("c63-restart-child", || async move {
        let root = PathBuf::from(root);
        let requested_stage = std::env::var("AUDEX_C63_RESTART_STAGE")?;
        let stage = if matches!(requested_stage.as_str(), "unreadable" | "reused") {
            "captured".to_owned()
        } else {
            requested_stage.clone()
        };
        let recovering = std::env::var_os("AUDEX_C63_RECOVER").is_some();
        let server = responses::start_mock_server().await;
        if recovering {
            let saved: Value =
                serde_json::from_slice(&std::fs::read(root.join("checkpoint.json"))?)?;
            let home = Path::new(saved["home"].as_str().unwrap());
            let (controller, threads) = fixture_at_path(
                home,
                &server.uri(),
                CodexAuth::from_api_key("dummy"),
                false,
                Arc::new(|_, _| Box::pin(async { Ok(()) })),
                Default::default(),
                false,
            )
            .await;
            let store = controller.store().await?;
            let id = saved["run_id"].as_str().unwrap();
            let operation = saved["operation"].as_str().unwrap();
            let original = store.client_launch(operation).await?.unwrap();
            if requested_stage == "unreadable" {
                let marker = Path::new(&original.mcp_config_path)
                    .parent()
                    .unwrap()
                    .join("c63-identity-unreadable");
                std::fs::write(&marker, "injected read failure")?;
                controller.tick().await?;
                let view = store.local_execution_view(operation).await?.unwrap();
                assert_eq!(view.status, "local_identity_unconfirmed");
                assert!(
                    view.stop_confirmed_at_ms.is_none() && view.cleanup_finished_at_ms.is_none()
                );
                assert_eq!(
                    store.client_launch(operation).await?.unwrap().execution_id,
                    original.execution_id
                );
                std::fs::remove_file(marker)?;
            }
            let clean =
                super::lifecycle_qualification::reconcile_until(&controller, operation).await?;
            for _ in 0..3 {
                controller.tick().await?;
            }
            assert_eq!(
                store.client_launch(operation).await?.unwrap().execution_id,
                original.execution_id
            );
            assert_eq!(store.unfinished_client_launches().await?.len(), 0);
            let run = store.load_run(id).await?;
            if matches!(stage.as_str(), "terminal_persisted" | "accepted") {
                assert_eq!(run.assignment_state, AssignmentState::Succeeded);
                assert!(store.client_terminal_result(operation).await?.is_some());
            } else {
                assert_ne!(run.assignment_state, AssignmentState::Succeeded);
            }
            if stage == "accepted" {
                assert_eq!(read_report_identity(&store, id).await?, saved["report"]);
            }
            let count = std::fs::read_to_string(home.join("stream-launches"))
                .unwrap_or_default()
                .lines()
                .count();
            assert_eq!(
                count,
                usize::from(matches!(
                    stage.as_str(),
                    "captured" | "terminal_persisted" | "accepted"
                ))
            );
            std::fs::write(root.join("recovered.json"), json!({"stage":stage,"execution":original.execution_id,
                "prompt_count":count,"cleanup":clean,"completed":run.assignment_state==AssignmentState::Succeeded}).to_string())?;
            threads
                .shutdown_all_threads_bounded(Duration::from_secs(5))
                .await;
            return Ok(());
        }
        let temp = tempfile::tempdir()?;
        let checkpoint_home = temp.path().to_path_buf();
        let checkpoint_root = root.clone();
        let checkpoint_stage = stage.clone();
        let hooks = crate::audex::dispatch::DispatchTestHooks {
            checkpoint: Some(Arc::new(move |name| {
                let (root, home, stage) = (
                    checkpoint_root.clone(),
                    checkpoint_home.clone(),
                    checkpoint_stage.clone(),
                );
                Box::pin(async move {
                    if name == stage {
                        checkpoint(&root, &home, &stage).await.unwrap();
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
        let id = seed_stream(&controller, &temp, "valid", true).await?;
        std::fs::write(
            temp.path().join("stream-scenario.json"),
            json!({"mode":"valid","gate":true,"lifecycle":true}).to_string(),
        )?;
        controller.drive(&id).await?;
        receipt(temp.path(), "lifecycle-ready.json").await?;
        let store = controller.store().await?;
        let run = store.load_run(&id).await?;
        super::lifecycle_qualification::observed_socket(&controller, &run.operation_id).await?;
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                controller.drive(&id).await?;
                if !store.activity(&run.assignment_id, 0, 100).await?.is_empty() {
                    break anyhow::Ok(());
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await??;
        if stage == "captured" {
            checkpoint(&root, temp.path(), &stage).await?;
        } else {
            std::fs::write(temp.path().join("before-terminal.release"), "go")?;
            if stage == "accepted" {
                assert_eq!(
                    settled(&controller, &id).await?.assignment_state,
                    AssignmentState::Succeeded
                );
                finished(&controller, &id).await;
                checkpoint(&root, temp.path(), &stage).await?;
            }
        }
        std::future::pending::<()>().await;
        Ok(())
    })
}

#[test]
fn daemon_restart_reconciles_every_durable_boundary_without_replay() -> anyhow::Result<()> {
    run_test_with_large_stack("c63-restart-matrix", || async {
        for stage in [
            "launch_intent",
            "child_identity",
            "prompt_write_started",
            "captured",
            "terminal_persisted",
            "accepted",
            "unreadable",
            "reused",
        ] {
            let root = tempfile::tempdir()?;
            let output = tokio::process::Command::new("/usr/bin/python3")
                .arg("-c")
                .arg(include_str!("../../../../external/restart_fixture.py"))
                .arg(std::env::current_exe()?)
                .arg(root.path())
                .arg(stage)
                .kill_on_drop(true)
                .output()
                .await?;
            anyhow::ensure!(
                output.status.success(),
                "restart {stage}: {} {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            let recovered: Value =
                serde_json::from_slice(&std::fs::read(root.path().join("recovered.json"))?)?;
            println!(
                "C63 separate-daemon restart {stage}: original execution={}, prompts={}, complete={}; verified survivors stopped and cleanup reconciled",
                recovered["execution"], recovered["prompt_count"], recovered["completed"]
            );
        }
        Ok(())
    })
}
