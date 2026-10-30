use super::*;

#[test]
fn retained_policy_allows_cold_collection_after_provider_and_catalogue_changes()
-> anyhow::Result<()> {
    run_test_with_large_stack("audex-immutable-observation-policy", || async {
        for remove_provider in [false, true] {
            let server = responses::start_mock_server().await;
            let temp = tempfile::tempdir()?;
            let (first, threads) = real_fixture(&temp, &server.uri()).await;
            let id = start(&first, "policy-retained").await;
            let run = first.store().await?.load_run(&id).await?;
            let raw = String::from_utf8(empty_result(&run))?;
            responses::mount_sse_once(
                &server,
                sse(vec![
                    ev_assistant_message("result", &raw),
                    ev_completed("one"),
                ]),
            )
            .await;
            first.drive(&id).await?;
            let thread = execution_thread(&first, &id).await;
            super::recovery::wait_completed(&threads, thread).await?;
            threads
                .shutdown_all_threads_bounded(Duration::from_secs(5))
                .await;
            super::recovery::expire(&first, &run.operation_id).await?;
            let mut changed = (*first.config).clone();
            std::fs::write(
                temp.path().join("config.toml"),
                "developer_instructions = 'Configuration file changed after submission'\nmodel_reasoning_effort = 'low'\n",
            )?;
            changed.developer_instructions =
                Some("Changed ambient configuration must not affect observation".into());
            if remove_provider {
                changed.model_providers.remove(&changed.model_provider_id);
            }
            let state = codex_state::StateRuntime::init(
                changed.sqlite_config().clone(),
                changed.model_provider_id.clone(),
            )
            .await?;
            let history = thread_store_from_config(&changed, Some(state));
            let mut model = codex_models_manager::model_info::model_info_from_slug("audit-test");
            model.display_name = "Changed catalogue metadata after submission".into();
            let models = Arc::new(StaticModelsManager::new(
                None,
                ModelsResponse {
                    models: vec![model],
                },
                ModelOrigin::ExplicitConfig,
            ));
            let recovered_threads = Arc::new(ThreadManager::new(
                &changed,
                codex_core::test_support::auth_manager_from_auth(CodexAuth::from_api_key("dummy")),
                models.clone(),
                codex_core::CodexAppsToolsCache::default(),
                codex_protocol::protocol::SessionSource::Exec,
                Arc::new(EnvironmentManager::default_for_tests()),
                Arc::new(codex_extension_api::ExtensionRegistryBuilder::new().build()),
                Arc::new(codex_core::test_support::EmptyUserInstructionsProvider),
                None,
                codex_core::passthrough_image_store(),
                history.clone(),
                None,
                "audex-test".into(),
                None,
                None,
            ));
            let changed = Arc::new(changed);
            let dispatch = Arc::new(CodexDispatch::new(
                changed.clone(),
                recovered_threads.clone(),
                history,
                Arc::new(|_, _| Box::pin(async { Ok(()) })),
            ));
            let recovered = AuditController::new(changed, models, dispatch, Arc::new(|_| {}));
            assert_eq!(finished(&recovered, &id).await["phase"], "completed");
            assert_eq!(server.received_requests().await.expect("requests").len(), 1);
            assert!(
                recovered_threads.list_thread_ids().await.is_empty(),
                "observation does not reconstruct or replace an execution"
            );
        }
        Ok(())
    })
}

#[test]
fn cancellation_after_configuration_change_uses_existing_runtime_and_retained_identity()
-> anyhow::Result<()> {
    run_test_with_large_stack("audex-cancel-retained-identity", || async {
        let server = responses::start_mock_server().await;
        let temp = tempfile::tempdir()?;
        let (first, threads) = real_fixture(&temp, &server.uri()).await;
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse(vec![ev_completed("delayed")]))
                    .set_delay(Duration::from_secs(60)),
            )
            .mount(&server)
            .await;
        let id = start(&first, "cancel-policy-drift").await;
        first.drive(&id).await?;
        let mut changed = (*first.config).clone();
        changed.model_providers.clear();
        let history = thread_store_from_config(&changed, None);
        let dispatch = Arc::new(CodexDispatch::new(
            Arc::new(changed),
            threads.clone(),
            history,
            Arc::new(|_, _| Box::pin(async { Ok(()) })),
        ));
        let observer = AuditController::new(
            first.config.clone(),
            first.models.clone(),
            dispatch,
            Arc::new(|_| {}),
        );
        let run = first.store().await?.load_run(&id).await?;
        super::recovery::expire(&first, &run.operation_id).await?;
        api(&observer, "audit/cancel", json!({"runId":id}))
            .await
            .expect("cancel intent");
        assert_eq!(finished(&observer, &id).await["phase"], "cancelled");
        assert!(server.received_requests().await.expect("requests").len() <= 1);
        threads
            .shutdown_all_threads_bounded(Duration::from_secs(5))
            .await;
        Ok(())
    })
}

#[test]
fn missing_session_metadata_is_pending_but_foreign_metadata_is_terminal_unknown()
-> anyhow::Result<()> {
    run_test_with_large_stack("audex-history-identity-evidence", || async {
        for foreign in [false, true] {
            let server = responses::start_mock_server().await;
            let temp = tempfile::tempdir()?;
            let change_history = Arc::new(AtomicBool::new(false));
            let flag = change_history.clone();
            let hooks = crate::audex::dispatch::DispatchTestHooks {
                history: Some(Arc::new(move |mut history| {
                    let change = flag.load(Ordering::SeqCst);
                    Box::pin(async move {
                        if change {
                            if foreign {
                                for item in &mut history.items {
                                    if let codex_rollout::RolloutItem::SessionMeta(meta) = item {
                                        meta.meta.id = ThreadId::new();
                                    }
                                }
                            } else {
                                history.items.retain(|item| {
                                    !matches!(item, codex_rollout::RolloutItem::SessionMeta(_))
                                });
                            }
                        }
                        Ok(history)
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
            let id = start(&controller, "history-identity").await;
            let store = controller.store().await?;
            let run = store.load_run(&id).await?;
            responses::mount_sse_once(
                &server,
                sse(vec![
                    ev_assistant_message("answer", &String::from_utf8(empty_result(&run))?),
                    ev_completed("one"),
                ]),
            )
            .await;
            controller.drive(&id).await?;
            super::recovery::wait_completed(&threads, execution_thread(&controller, &id).await)
                .await?;
            change_history.store(true, Ordering::SeqCst);
            controller.drive(&id).await?;
            let observed = store.load_run(&id).await?;
            if foreign {
                assert_eq!(observed.operation_state, OperationState::Unknown);
                assert!(!observed.observation_pending);
                assert!(
                    observed
                        .outcome
                        .expect("diagnostic")
                        .contains("foreign_persisted_thread")
                );
            } else {
                assert_eq!(observed.operation_state, OperationState::Accepted);
                assert!(observed.observation_pending);
                change_history.store(false, Ordering::SeqCst);
                assert_eq!(finished(&controller, &id).await["phase"], "completed");
            }
            assert_eq!(server.received_requests().await.expect("requests").len(), 1);
            threads
                .shutdown_all_threads_bounded(Duration::from_secs(5))
                .await;
        }
        Ok(())
    })
}
