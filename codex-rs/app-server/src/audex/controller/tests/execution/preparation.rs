use super::*;
use crate::audex::dispatch::DispatchTestHooks;

#[test]
fn repeated_live_observation_does_not_rewrite_acceptance_or_unchanged_usage() -> anyhow::Result<()>
{
    run_test_with_large_stack("audex-observation-writes", || async {
        let temp = tempfile::tempdir()?;
        let server = responses::start_mock_server().await;
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
        let (controller, threads) = real_fixture(&temp, &server.uri()).await;
        let id = start(&controller, "observation-writes").await;
        controller.drive(&id).await?;
        tokio::time::timeout(Duration::from_secs(10), async {
            while server
                .received_requests()
                .await
                .expect("requests")
                .is_empty()
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await?;
        controller.drive(&id).await?; // flush and retain the initial token observation
        let store = controller.store().await?;
        let run = store.load_run(&id).await?;
        let pool = store.pool_for_tests();
        sqlx::query("CREATE TABLE write_counter (kind TEXT)")
            .execute(&pool)
            .await?;
        sqlx::query("CREATE TRIGGER count_acceptance AFTER UPDATE ON audit_execution BEGIN INSERT INTO write_counter VALUES ('acceptance'); END").execute(&pool).await?;
        sqlx::query("CREATE TRIGGER count_usage AFTER UPDATE OF usage_json ON reviewer_assignments BEGIN INSERT INTO write_counter VALUES ('usage'); END").execute(&pool).await?;
        for _ in 0..3 {
            controller.drive(&id).await?;
        }
        let writes: i64 = sqlx::query_scalar("SELECT count(*) FROM write_counter")
            .fetch_one(&pool)
            .await?;
        assert_eq!(
            writes, 0,
            "unchanged positive observations must not rewrite durable acceptance or usage"
        );
        for _ in 0..3 {
            store
                .execution_usage(&run.operation_id, "{\"total\":1}")
                .await?;
        }
        let writes: i64 = sqlx::query_scalar("SELECT count(*) FROM write_counter")
            .fetch_one(&pool)
            .await?;
        assert_eq!(writes, 1);
        store
            .execution_usage(&run.operation_id, "{\"total\":2}")
            .await?;
        let writes: i64 = sqlx::query_scalar("SELECT count(*) FROM write_counter")
            .fetch_one(&pool)
            .await?;
        assert_eq!(writes, 2);
        assert!(server.received_requests().await.expect("requests").len() <= 1);
        threads
            .shutdown_all_threads_bounded(Duration::from_secs(5))
            .await;
        Ok(())
    })
}

#[test]
fn failed_listener_installation_cleans_the_unsubmitted_thread_and_listener() -> anyhow::Result<()> {
    run_test_with_large_stack("audex-failed-listener", || async {
        let temp = tempfile::tempdir()?;
        let server = responses::start_mock_server().await;
        let live = Arc::new(AtomicBool::new(false));
        let installed = live.clone();
        let cleared = live.clone();
        let listen: crate::audex::dispatch::Listen = Arc::new(move |_, _| {
            installed.store(true, Ordering::SeqCst);
            Box::pin(async { anyhow::bail!("injected partial listener failure") })
        });
        let hooks = DispatchTestHooks {
            cleanup: Some(Arc::new(move |_, thread| {
                assert!(thread.has_shutdown_status());
                cleared.store(false, Ordering::SeqCst);
                Box::pin(async { Ok(()) })
            })),
            ..Default::default()
        };
        let (controller, threads) = fixture_with_hooks(
            &temp,
            &server.uri(),
            CodexAuth::from_api_key("dummy"),
            false,
            listen,
            hooks,
        )
        .await;
        let id = start(&controller, "partial-listener").await;
        controller.drive(&id).await?;
        assert!(!live.load(Ordering::SeqCst));
        assert!(threads.list_thread_ids().await.is_empty());
        assert_eq!(
            controller
                .store()
                .await?
                .load_run(&id)
                .await?
                .assignment_state,
            AssignmentState::Failed
        );
        assert!(
            server
                .received_requests()
                .await
                .expect("requests")
                .is_empty()
        );
        Ok(())
    })
}

#[test]
fn abandoned_unsubmitted_runtime_is_fenced_then_cleaned_by_recovery() -> anyhow::Result<()> {
    run_test_with_large_stack("audex-abandoned-preparation", || async {
        let temp = tempfile::tempdir()?;
        let server = responses::start_mock_server().await;
        let entered = Arc::new(Notify::new());
        let gate = Arc::new(Notify::new());
        let (signal, wait) = (entered.clone(), gate.clone());
        let hooks = DispatchTestHooks {
            checkpoint: Some(Arc::new(move |stage| {
                let (signal, wait) = (signal.clone(), wait.clone());
                Box::pin(async move {
                    if stage == "history_materialised" {
                        signal.notify_one();
                        wait.notified().await;
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
        let id = start(&controller, "abandoned-preparation").await;
        let (driver, run_id) = (controller.clone(), id.clone());
        let task = tokio::spawn(async move { driver.drive(&run_id).await });
        entered.notified().await;
        let run = controller.store().await?.load_run(&id).await?;
        let thread = execution_thread(&controller, &id).await;
        task.abort();
        assert!(task.await.expect_err("aborted preparer").is_cancelled());
        recovery::expire(&controller, &run.operation_id).await?;
        controller.drive(&id).await?;
        assert_eq!(
            controller
                .store()
                .await?
                .load_run(&id)
                .await?
                .assignment_state,
            AssignmentState::Failed
        );
        assert!(
            threads.get_thread(thread).await.is_err(),
            "a definitely unsubmitted abandoned runtime must be removed"
        );
        assert!(
            server
                .received_requests()
                .await
                .expect("requests")
                .is_empty()
        );
        Ok(())
    })
}

#[test]
fn preparation_renews_only_its_unexpired_lease_and_cancellation_fences_it() -> anyhow::Result<()> {
    run_test_with_large_stack("audex-preparation-lease", || async {
        let temp = tempfile::tempdir()?;
        let server = responses::start_mock_server().await;
        let entered = Arc::new(Notify::new());
        let gate = Arc::new(Notify::new());
        let (signal, wait) = (entered.clone(), gate.clone());
        let hooks = DispatchTestHooks {
            checkpoint: Some(Arc::new(move |stage| {
                let (signal, wait) = (signal.clone(), wait.clone());
                Box::pin(async move {
                    if stage == "history_materialised" {
                        signal.notify_one();
                        wait.notified().await;
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
        let id = start(&controller, "slow-preparation").await;
        let (driver, run_id) = (controller.clone(), id.clone());
        let task = tokio::spawn(async move { driver.drive(&run_id).await });
        entered.notified().await;
        let store = controller.store().await?;
        let run = store.load_run(&id).await?;
        let expiry: i64 =
            sqlx::query_scalar("SELECT lease_expires_at_ms FROM operations WHERE id=?")
                .bind(&run.operation_id)
                .fetch_one(&store.pool_for_tests())
                .await?;
        tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                let next: i64 =
                    sqlx::query_scalar("SELECT lease_expires_at_ms FROM operations WHERE id=?")
                        .bind(&run.operation_id)
                        .fetch_one(&store.pool_for_tests())
                        .await?;
                if next > expiry {
                    return anyhow::Ok(());
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await??;
        assert!(
            !store
                .renew_preparation_lease(
                    &run.operation_id,
                    "foreign-owner",
                    chrono::Utc::now().timestamp_millis()
                )
                .await?
        );
        recovery::expire(&controller, &run.operation_id).await?;
        assert!(
            !store
                .renew_preparation_lease(
                    &run.operation_id,
                    &controller.owner,
                    chrono::Utc::now().timestamp_millis()
                )
                .await?,
            "an expired owner cannot revive its lease"
        );
        api(&controller, "audit/cancel", json!({"runId":id}))
            .await
            .expect("cancel");
        assert!(
            !store
                .renew_preparation_lease(
                    &run.operation_id,
                    &controller.owner,
                    chrono::Utc::now().timestamp_millis()
                )
                .await?
        );
        gate.notify_one();
        task.await??;
        assert!(threads.list_thread_ids().await.is_empty());
        assert_eq!(store.load_run(&id).await?.phase, RunPhase::Cancelled);
        assert!(
            server
                .received_requests()
                .await
                .expect("requests")
                .is_empty()
        );
        Ok(())
    })
}
