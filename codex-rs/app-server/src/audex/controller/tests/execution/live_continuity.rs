//! Opt-in commercial qualification. Generated source only; never part of default test runs.
use super::*;
use crate::audex::dispatch::DispatchTestHooks;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

#[test]
#[ignore = "uses genuine commercial credentials and inference; M5 qualification only"]
fn genuine_provider_context_continuity() -> anyhow::Result<()> {
    run_test_with_large_stack("m5-live", || async {
        let mode = std::env::var("AUDEX_M5_LIVE_MODE")?;
        anyhow::ensure!(
            [
                "native-local",
                "native-remote",
                "native-background",
                "claude"
            ]
            .contains(&mode.as_str()),
            "unknown qualification mode"
        );
        let claude = mode.starts_with("claude");
        let background = mode == "native-background";
        let local = mode == "native-local";
        let temp = tempfile::tempdir()?;
        let auth = if claude {
            CodexAuth::from_api_key("unused-native-fixture")
        } else {
            let source = PathBuf::from(std::env::var("AUDEX_M5_AUTH_HOME")?);
            let auth_home = temp.path().join("qualification-auth");
            std::fs::create_dir(&auth_home)?;
            std::fs::set_permissions(&auth_home, std::fs::Permissions::from_mode(0o700))?;
            std::fs::copy(source.join("auth.json"), auth_home.join("auth.json"))?;
            CodexAuth::from_auth_storage(
                &auth_home,
                codex_login::AuthCredentialsStoreMode::File,
                None,
                Default::default(),
                &codex_login::test_support::transport_default_auth_route_config(),
            )
            .await?
            .ok_or_else(|| anyhow::anyhow!("native qualification credential unavailable"))?
        };
        let hooks = DispatchTestHooks {
            config: Some(Arc::new(move |config| {
                config.model_provider.base_url = None;
                config
                    .model_provider
                    .capabilities
                    .get_or_insert_with(Default::default)
                    .remote_compaction = Some(if local {
                    codex_model_provider_info::RemoteCompactionSupport::Unsupported
                } else {
                    codex_model_provider_info::RemoteCompactionSupport::V2
                });
            })),
            ..Default::default()
        };
        let config =
            continuity::context_config(&temp, background, if background { 70_000 } else { 12_000 })
                .await?;
        let (controller, threads) = fixture_from_config(
            config,
            "https://unused.invalid",
            auth,
            false,
            admission::native_reader(),
            hooks,
            false,
        )
        .await;
        std::fs::create_dir_all(controller.root.as_path())?;
        if claude {
            let settings: Value =
                serde_json::from_slice(&std::fs::read(std::env::var("AUDEX_M5_BACKENDS")?)?)?;
            // Reuse the configured client's credential-refresh path. Copying an
            // expired OAuth credential can rotate its refresh token in a disposable
            // profile and strand the configured profile when the fixture is removed.
            let mut settings = settings;
            settings["claudeSubscription"]["runtimeRoot"] =
                json!(std::env::var("AUDEX_CLIENT_TEST_RUNTIME_ROOT")?);
            std::fs::write(
                controller.root.join("reviewer-backends.json"),
                serde_json::to_vec(&settings)?,
            )?;
            // Official client control, isolated to this compiled qualification fixture.
            std::fs::write(controller.root.join("m5-compaction-percent"), "10")?;
        }
        let mut source =
            String::from("// Generated M5 continuity fixture. Each function is independent.\n");
        for index in 0..1800 {
            source.push_str(&format!("pub fn sample_{index}(n: u32) -> u32 {{ let divisor = {}; let adjusted = n.wrapping_add({}); adjusted / divisor }}\n",index%97+1,index*31));
        }
        source.push_str("pub fn final_defect(n: u32) -> u32 { n / (n - n) }\n");
        let reviewer = if claude {
            json!({"provider":"anthropic","model":"claude-haiku-5-5","executionAdapter":"claude_client","authenticationRoute":"claude_subscription"})
        } else {
            json!({"provider":"openai","model":"gpt-6-luna"})
        };
        let mut params = json!({"requestKey":format!("m5-live-{mode}"),"source":{"type":"snippet","name":"example.rs","content":source},
            "brief":"Review all of this generated Rust source. Read the complete captured file in sequential 60000-byte chunks using the source tools, without skipping ranges, before concluding. Keep a short progress note with the last byte offset after every read so work survives automatic compaction. Check arithmetic defects, especially the last function. Do not repeat already read ranges after compaction. Return a concise final review bound to the supplied assignment and snapshot.","reviewers":[reviewer]});
        params["preflight"] = api(&controller, "audit/preflight", json!({"request":params}))
            .await
            .map_err(|e| anyhow::anyhow!("preflight: {}", e.message))?;
        let started = api(&controller, "audit/start", params)
            .await
            .map_err(|e| anyhow::anyhow!("commission: {}", e.message))?;
        let id = started["run"]["runId"].as_str().unwrap().to_owned();
        let result = tokio::time::timeout(Duration::from_secs(900), async {
            loop {
                controller.drive(&id).await?;
                let run = controller.store().await?.load_run_metadata(&id).await?;
                if matches!(
                    run.assignment_state,
                    AssignmentState::Succeeded | AssignmentState::Failed | AssignmentState::Unknown
                ) {
                    break anyhow::Ok(run);
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        })
        .await;
        let store = controller.store().await?;
        let run = store.load_run_metadata(&id).await?;
        let attempt = controller
            .read_attempt(AuditAttemptReadParams {
                run_id: id.clone(),
                assignment_id: run.assignment_id.clone(),
                after_sequence: None,
                limit: Some(256),
            })
            .await?;
        let mut activity = store.activity(&run.assignment_id, 0, 256).await?;
        loop {
            let after = activity.last().map_or(0, |entry| entry.sequence);
            let page = store.activity(&run.assignment_id, after, 256).await?;
            if page.is_empty() {
                break;
            }
            activity.extend(page);
        }
        let compactions = activity
            .iter()
            .filter(|event| {
                event.kind == "status" && event.payload == "client_compaction_completed"
            })
            .count();
        let evidence = PathBuf::from(std::env::var("AUDEX_M5_EVIDENCE_ROOT")?);
        std::fs::create_dir_all(&evidence)?;
        std::fs::write(
            evidence.join(format!("{mode}.json")),
            serde_json::to_vec_pretty(
                &json!({"mode":mode,"runId":id,"snapshotId":run.snapshot_id,
            "assignmentId":run.assignment_id,"compactions":compactions,"attempt":attempt,"activity":activity,
            "failure":recovery::issue(&controller,&run.operation_id).await?}),
            )?,
        )?;
        let succeeded = run.assignment_state == AssignmentState::Succeeded;
        if !succeeded {
            controller.abort_audit(&id).await?;
        } else {
            controller.drive(&id).await?;
        }
        if claude {
            external::lifecycle_qualification::reconcile_until(&controller, &run.operation_id)
                .await?;
        }
        threads
            .shutdown_all_threads_bounded(Duration::from_secs(10))
            .await;
        anyhow::ensure!(
            result.is_ok(),
            "qualification timed out; see retained evidence"
        );
        result.unwrap()?;
        anyhow::ensure!(
            succeeded,
            "genuine reviewer did not complete; see retained evidence"
        );
        anyhow::ensure!(
            compactions > 0,
            "genuine run did not demonstrate compaction"
        );
        anyhow::ensure!(
            store.attempts(&id).await?.len() == 1,
            "unexpected independent replacement during qualification"
        );
        println!(
            "M5 genuine {mode}: succeeded={succeeded}, compactions={compactions}, same audit and attempt; billing not inferred"
        );
        Ok(())
    })
}
