use super::*;
use crate::audex::dispatch::DispatchTestHooks;
use crate::audex::store::SubmissionBoundary;
use codex_protocol::protocol::{AgentStatus, EventMsg};
use codex_rollout::RolloutItem;
use codex_thread_store::ThreadStoreError;
use std::sync::atomic::AtomicUsize;

async fn with_hooks(
    temp: &tempfile::TempDir,
    url: &str,
    hooks: DispatchTestHooks,
) -> (Arc<AuditController>, Arc<ThreadManager>) {
    fixture_with_hooks(
        temp,
        url,
        CodexAuth::from_api_key("dummy"),
        false,
        Arc::new(|_, _| Box::pin(async { Ok(()) })),
        hooks,
    )
    .await
}

async fn expire(controller: &AuditController, operation: &str) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE operations SET lease_expires_at_ms=0 WHERE id=? AND lease_owner IS NOT NULL",
    )
    .bind(operation)
    .execute(&controller.store().await?.pool_for_tests())
    .await?;
    Ok(())
}

async fn issue(
    controller: &AuditController,
    operation: &str,
) -> anyhow::Result<(Option<String>, Option<String>)> {
    Ok(
        sqlx::query_as("SELECT error,retry_classification FROM operations WHERE id=?")
            .bind(operation)
            .fetch_one(&controller.store().await?.pool_for_tests())
            .await?,
    )
}

async fn wait_completed(threads: &ThreadManager, thread: ThreadId) -> anyhow::Result<()> {
    let thread = threads.get_thread(thread).await?;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if matches!(thread.agent_status().await, AgentStatus::Completed(_)) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await?;
    Ok(())
}

fn hide_terminal(history: &mut codex_thread_store::StoredModelContext) {
    history.items.retain(|item| {
        !matches!(
            item,
            RolloutItem::EventMsg(EventMsg::TurnComplete(_) | EventMsg::TurnAborted(_))
        )
    });
}

#[test]
fn startup_crash_windows_preserve_the_submission_boundary_without_resampling() -> anyhow::Result<()>
{
    run_test_with_large_stack("audex-submission-crash-windows", || async {
        for stage in [
            "reserved",
            "thread_created",
            "history_materialised",
            "submission_started",
            "core_accepted",
            "acceptance_saved",
        ] {
            let server = responses::start_mock_server().await;
            let temp = tempfile::tempdir()?;
            let entered = Arc::new(Notify::new());
            let release = Arc::new(Notify::new());
            let (signal, gate) = (entered.clone(), release.clone());
            let hooks = DispatchTestHooks {
                checkpoint: Some(Arc::new(move |at| {
                    let (signal, gate) = (signal.clone(), gate.clone());
                    Box::pin(async move {
                        if at == stage {
                            signal.notify_one();
                            gate.notified().await;
                        }
                    })
                })),
                ..Default::default()
            };
            let (controller, threads) = with_hooks(&temp, &server.uri(), hooks).await;
            let id = start(&controller, stage).await;
            let store = controller.store().await?;
            let run = store.load_run(&id).await?;
            let submitted = matches!(stage, "core_accepted" | "acceptance_saved");
            if submitted {
                let raw = String::from_utf8(empty_result(&run))?;
                responses::mount_sse_once(
                    &server,
                    sse(vec![
                        ev_response_created("one"),
                        ev_assistant_message("answer", &raw),
                        ev_completed("one"),
                    ]),
                )
                .await;
            }
            let (driver, run_id) = (controller.clone(), id.clone());
            let task = tokio::spawn(async move { driver.drive(&run_id).await });
            tokio::time::timeout(Duration::from_secs(10), entered.notified()).await?;
            let boundary = store.submission_boundary(&run.operation_id).await?;
            assert_eq!(
                boundary,
                match stage {
                    "submission_started" | "core_accepted" => SubmissionBoundary::SubmissionStarted,
                    "acceptance_saved" => SubmissionBoundary::Accepted,
                    _ => SubmissionBoundary::Unsubmitted,
                },
                "{stage}"
            );
            task.abort();
            assert!(task.await.expect_err("cancelled driver").is_cancelled());
            if submitted {
                wait_completed(&threads, execution_thread(&controller, &id).await).await?;
            }
            threads
                .shutdown_all_threads_bounded(Duration::from_secs(5))
                .await;
            expire(&controller, &run.operation_id).await?;
            let (recovered, recovered_threads) = real_fixture(&temp, &server.uri()).await;
            recovered.drive(&id).await?;
            if boundary == SubmissionBoundary::Unsubmitted {
                let retained = recovered.store().await?.load_run(&id).await?;
                assert_eq!(retained.assignment_state, AssignmentState::Failed);
                assert!(
                    retained
                        .outcome
                        .expect("known local failure")
                        .contains("interrupted_before_submission")
                );
            } else if submitted {
                assert_eq!(finished(&recovered, &id).await["phase"], "completed");
            } else {
                let retained = recovered.store().await?.load_run(&id).await?;
                assert_eq!(retained.phase, RunPhase::Reviewing);
                assert_eq!(retained.operation_state, OperationState::Dispatching);
                let (error, retry) = issue(&recovered, &run.operation_id).await?;
                assert!(
                    error
                        .expect("submission uncertainty")
                        .contains("turn_acceptance_unconfirmed")
                );
                assert_eq!(retry.as_deref(), Some("observe_only"));
            }
            for _ in 0..3 {
                recovered.tick().await?;
            }
            assert_eq!(
                server.received_requests().await.expect("requests").len(),
                usize::from(submitted),
                "{stage}"
            );
            assert!(
                recovered_threads.list_thread_ids().await.is_empty(),
                "recovery must not instantiate a replacement reviewer"
            );
        }
        Ok(())
    })
}

#[test]
fn expired_startup_owner_cannot_cross_the_submission_marker() -> anyhow::Result<()> {
    run_test_with_large_stack("audex-expired-submission-owner", || async {
        let server = responses::start_mock_server().await;
        let temp = tempfile::tempdir()?;
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let (signal, gate) = (entered.clone(), release.clone());
        let hooks = DispatchTestHooks {
            checkpoint: Some(Arc::new(move |stage| {
                let (signal, gate) = (signal.clone(), gate.clone());
                Box::pin(async move {
                    if stage == "history_materialised" {
                        signal.notify_one();
                        gate.notified().await;
                    }
                })
            })),
            ..Default::default()
        };
        let (controller, threads) = with_hooks(&temp, &server.uri(), hooks).await;
        let id = start(&controller, "expired").await;
        let run = controller.store().await?.load_run(&id).await?;
        let (driver, run_id) = (controller.clone(), id.clone());
        let task = tokio::spawn(async move { driver.drive(&run_id).await });
        tokio::time::timeout(Duration::from_secs(10), entered.notified()).await?;
        expire(&controller, &run.operation_id).await?;
        release.notify_one();
        task.await??;
        let retained = controller.store().await?.load_run(&id).await?;
        assert_eq!(retained.assignment_state, AssignmentState::Failed);
        assert!(
            retained
                .outcome
                .expect("admission diagnosis")
                .contains("submission_admission")
        );
        assert_eq!(
            controller
                .store()
                .await?
                .submission_boundary(&run.operation_id)
                .await?,
            SubmissionBoundary::Unsubmitted
        );
        assert!(
            server
                .received_requests()
                .await
                .expect("requests")
                .is_empty()
        );
        threads
            .shutdown_all_threads_bounded(Duration::from_secs(5))
            .await;
        Ok(())
    })
}

#[test]
fn preflight_failure_is_durably_local_and_never_submitted() -> anyhow::Result<()> {
    run_test_with_large_stack("audex-local-preflight-failure", || async {
        let server = responses::start_mock_server().await;
        let temp = tempfile::tempdir()?;
        let (first, threads) = real_fixture(&temp, &server.uri()).await;
        let id = start(&first, "configuration-drift").await;
        let run = first.store().await?.load_run(&id).await?;
        let mut changed = (*first.config).clone();
        changed.model_providers.clear();
        changed.model_provider_id = "removed-provider".to_string();
        let dispatch = Arc::new(CodexDispatch::new(
            Arc::new(changed.clone()),
            threads.clone(),
            thread_store_from_config(&changed, None),
            Arc::new(|_, _| Box::pin(async { Ok(()) })),
        ));
        let controller = AuditController::new(
            first.config.clone(),
            first.models.clone(),
            dispatch,
            Arc::new(|_| {}),
        );
        controller.drive(&id).await?;
        let retained = controller.store().await?.load_run(&id).await?;
        assert_eq!(retained.assignment_state, AssignmentState::Failed);
        assert_eq!(
            controller
                .store()
                .await?
                .submission_boundary(&run.operation_id)
                .await?,
            SubmissionBoundary::Unsubmitted
        );
        let (error, retry) = issue(&controller, &run.operation_id).await?;
        let error: Value = serde_json::from_str(&error.expect("durable diagnostic"))?;
        assert_eq!(error["stage"], "reviewer_preflight");
        assert_eq!(error["cause"], "unknown reviewer provider");
        assert_eq!(retry.as_deref(), Some("never_automatically_retry"));
        assert!(
            server
                .received_requests()
                .await
                .expect("requests")
                .is_empty()
        );
        assert!(threads.list_thread_ids().await.is_empty());
        Ok(())
    })
}

#[test]
fn preparation_and_acceptance_write_failures_keep_their_certainty_and_diagnostics()
-> anyhow::Result<()> {
    run_test_with_large_stack("audex-persistence-failures", || async {
        for stage in [
            "reserve_thread",
            "persist_submission_boundary",
            "persist_acceptance",
            "persist_usage",
        ] {
            let server = responses::start_mock_server().await;
            let temp = tempfile::tempdir()?;
            let (controller, threads) = real_fixture(&temp, &server.uri()).await;
            let id = start(&controller, stage).await;
            let store = controller.store().await?;
            let run = store.load_run(&id).await?;
            let pool = store.pool_for_tests();
            let trigger = match stage {
                "reserve_thread" => {
                    "CREATE TRIGGER injected BEFORE INSERT ON audit_execution BEGIN SELECT RAISE(ABORT,'sk-secret-never-retain'); END"
                }
                "persist_submission_boundary" => {
                    "CREATE TRIGGER injected BEFORE UPDATE OF submission_boundary ON operations WHEN NEW.submission_boundary='submission_started' BEGIN SELECT RAISE(ABORT,'sk-secret-never-retain'); END"
                }
                "persist_acceptance" => {
                    "CREATE TRIGGER injected BEFORE UPDATE OF turn_id ON audit_execution BEGIN SELECT RAISE(ABORT,'sk-secret-never-retain'); END"
                }
                _ => {
                    "CREATE TRIGGER injected BEFORE UPDATE OF usage_json ON reviewer_assignments BEGIN SELECT RAISE(ABORT,'sk-secret-never-retain'); END"
                }
            };
            sqlx::query(trigger).execute(&pool).await?;
            let submitted = matches!(stage, "persist_acceptance" | "persist_usage");
            if submitted {
                let raw = String::from_utf8(empty_result(&run))?;
                responses::mount_sse_once(
                    &server,
                    sse(vec![
                        ev_response_created("one"),
                        ev_assistant_message("answer", &raw),
                        ev_completed("one"),
                    ]),
                )
                .await;
            }
            controller.drive(&id).await?;
            if stage == "persist_usage" {
                wait_completed(&threads, execution_thread(&controller, &id).await).await?;
                controller.drive(&id).await?;
            }
            let retained = store.load_run(&id).await?;
            let (error, retry) = issue(&controller, &run.operation_id).await?;
            let text = error.expect("retained write diagnostic");
            let diagnostic: Value = serde_json::from_str(&text)?;
            assert_eq!(diagnostic["stage"], stage);
            assert!(
                diagnostic["cause"]
                    .as_str()
                    .expect("cause")
                    .starts_with("sqlite:")
            );
            assert!(!text.contains("sk-secret"));
            if submitted {
                assert_eq!(retained.phase, RunPhase::Reviewing);
                assert_eq!(retry.as_deref(), Some("observe_only"));
                controller.drive(&id).await?;
                assert_eq!(
                    store.load_run(&id).await?.revision,
                    retained.revision,
                    "duplicate diagnostic must be idempotent"
                );
            } else {
                assert_eq!(retained.assignment_state, AssignmentState::Failed);
                assert_eq!(
                    store.submission_boundary(&run.operation_id).await?,
                    SubmissionBoundary::Unsubmitted
                );
                assert_eq!(retry.as_deref(), Some("never_automatically_retry"));
            }
            sqlx::query("DROP TRIGGER injected").execute(&pool).await?;
            if submitted {
                assert_eq!(finished(&controller, &id).await["phase"], "completed");
            }
            for _ in 0..2 {
                controller.tick().await?;
            }
            assert_eq!(
                server.received_requests().await.expect("requests").len(),
                usize::from(submitted)
            );
            threads
                .shutdown_all_threads_bounded(Duration::from_secs(5))
                .await;
        }
        Ok(())
    })
}

#[test]
fn observation_errors_preserve_diagnostics_cancellation_and_the_original_sample()
-> anyhow::Result<()> {
    run_test_with_large_stack("audex-observation-errors", || async {
        for cancel in [false, true] {
            let server = responses::start_mock_server().await;
            let temp = tempfile::tempdir()?;
            let fail = Arc::new(AtomicBool::new(true));
            let injected = fail.clone();
            let hooks = DispatchTestHooks {
                history: Some(Arc::new(move |history| {
                    let failed = injected.load(Ordering::SeqCst);
                    Box::pin(async move {
                        if failed {
                            Err(ThreadStoreError::Retryable {
                                message: "sk-secret-history-error".to_string(),
                            })
                        } else {
                            Ok(history)
                        }
                    })
                })),
                ..Default::default()
            };
            let (controller, threads) = with_hooks(&temp, &server.uri(), hooks).await;
            let id = start(&controller, "history-error").await;
            let run = controller.store().await?.load_run(&id).await?;
            let raw = String::from_utf8(empty_result(&run))?;
            Mock::given(method("POST"))
                .and(path("/v1/responses"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .insert_header("content-type", "text/event-stream")
                        .set_body_string(sse(vec![
                            ev_response_created("one"),
                            ev_assistant_message("answer", &raw),
                            ev_completed("one"),
                        ]))
                        .set_delay(if cancel {
                            Duration::from_secs(30)
                        } else {
                            Duration::ZERO
                        }),
                )
                .expect(1)
                .mount(&server)
                .await;
            controller.drive(&id).await?;
            tokio::time::timeout(Duration::from_secs(10), async {
                while server
                    .received_requests()
                    .await
                    .expect("requests")
                    .is_empty()
                {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await?;
            if cancel {
                api(&controller, "audit/cancel", json!({"runId":id}))
                    .await
                    .expect("durable cancellation");
            } else {
                wait_completed(&threads, execution_thread(&controller, &id).await).await?;
            }
            controller.drive(&id).await?;
            let retained = controller.store().await?.load_run(&id).await?;
            let (text, retry) = issue(&controller, &run.operation_id).await?;
            let text = text.expect("diagnostic");
            let error: Value = serde_json::from_str(&text)?;
            assert_eq!(error["stage"], "read_history");
            assert_eq!(error["cause"], "thread_store_retryable");
            assert!(!text.contains("sk-secret"));
            assert_eq!(retry.as_deref(), Some("observe_only"));
            assert_eq!(retained.phase, RunPhase::Reviewing);
            assert_eq!(
                retained.control_intent,
                if cancel { "cancel_requested" } else { "active" }
            );
            controller.drive(&id).await?;
            assert_eq!(
                controller.store().await?.load_run(&id).await?.revision,
                retained.revision
            );
            fail.store(false, Ordering::SeqCst);
            assert_eq!(
                finished(&controller, &id).await["phase"],
                if cancel { "cancelled" } else { "completed" }
            );
            for _ in 0..3 {
                controller.tick().await?;
            }
            assert_eq!(server.received_requests().await.expect("requests").len(), 1);
            threads
                .shutdown_all_threads_bounded(Duration::from_secs(5))
                .await;
        }
        Ok(())
    })
}

#[test]
fn stale_unsubmitted_observation_cannot_close_a_submitting_operation() -> anyhow::Result<()> {
    run_test_with_large_stack("audex-stale-startup-observation", || async {
        for submission_wins in [false, true] {
            let server = responses::start_mock_server().await;
            let temp = tempfile::tempdir()?;
            let entered = Arc::new(Notify::new());
            let (signal, gate) = (entered.clone(), Arc::new(Notify::new()));
            let hooks = DispatchTestHooks {
                checkpoint: Some(Arc::new(move |stage| {
                    let (signal, gate) = (signal.clone(), gate.clone());
                    Box::pin(async move {
                        if stage == "history_materialised" {
                            signal.notify_one();
                            gate.notified().await;
                        }
                    })
                })),
                ..Default::default()
            };
            let (controller, threads) = with_hooks(&temp, &server.uri(), hooks).await;
            let id = start(&controller, "stale-startup").await;
            let (driver, run_id) = (controller.clone(), id.clone());
            let task = tokio::spawn(async move { driver.drive(&run_id).await });
            tokio::time::timeout(Duration::from_secs(10), entered.notified()).await?;
            let store = controller.store().await?;
            let observed = store.load_run(&id).await?;
            let snapshot = controller
                .snapshots()
                .open(&SnapshotId::parse(observed.snapshot_id.clone())?)
                .await?;
            let fail = || Observation::Unsubmitted("interrupted_before_submission".to_string());
            if submission_wins {
                assert!(
                    store
                        .begin_submission(
                            &observed.operation_id,
                            &controller.owner,
                            chrono::Utc::now().timestamp_millis()
                        )
                        .await?
                );
                controller
                    .reconcile_observation(&observed, &snapshot, fail())
                    .await?;
                assert_eq!(
                    store.load_run(&id).await?.operation_state,
                    OperationState::Dispatching
                );
                assert_eq!(store.load_run(&id).await?.revision, observed.revision);
            } else {
                controller
                    .reconcile_observation(&observed, &snapshot, fail())
                    .await?;
                assert!(
                    !store
                        .begin_submission(
                            &observed.operation_id,
                            &controller.owner,
                            chrono::Utc::now().timestamp_millis()
                        )
                        .await?
                );
                assert_eq!(
                    store.load_run(&id).await?.assignment_state,
                    AssignmentState::Failed
                );
            }
            task.abort();
            assert!(task.await.expect_err("stopped preparation").is_cancelled());
            threads
                .shutdown_all_threads_bounded(Duration::from_secs(5))
                .await;
            assert!(
                server
                    .received_requests()
                    .await
                    .expect("requests")
                    .is_empty()
            );
        }
        Ok(())
    })
}

#[test]
fn shutdown_between_history_reads_uses_a_fresh_durable_terminal_observation() -> anyhow::Result<()>
{
    run_test_with_large_stack("audex-shutdown-history-race", || async {
        let server = responses::start_mock_server().await;
        let temp = tempfile::tempdir()?;
        let holder = Arc::new(tokio::sync::OnceCell::<Arc<codex_core::CodexThread>>::new());
        let reads = Arc::new(AtomicUsize::new(0));
        let (runtime, count) = (holder.clone(), reads.clone());
        let hooks = DispatchTestHooks {
            history: Some(Arc::new(move |mut history| {
                let (runtime, count) = (runtime.clone(), count.clone());
                Box::pin(async move {
                    if count.fetch_add(1, Ordering::SeqCst) == 0 {
                        // The first read was captured before teardown finished.
                        runtime
                            .get()
                            .expect("runtime installed")
                            .shutdown_and_wait()
                            .await
                            .expect("shutdown");
                        hide_terminal(&mut history);
                    }
                    Ok(history)
                })
            })),
            ..Default::default()
        };
        let (controller, threads) = with_hooks(&temp, &server.uri(), hooks).await;
        let id = start(&controller, "shutdown-history-race").await;
        let run = controller.store().await?.load_run(&id).await?;
        let raw = String::from_utf8(empty_result(&run))?;
        responses::mount_sse_once(
            &server,
            sse(vec![
                ev_response_created("one"),
                ev_assistant_message("answer", &raw),
                ev_completed("one"),
            ]),
        )
        .await;
        controller.drive(&id).await?;
        let thread = execution_thread(&controller, &id).await;
        assert!(holder.set(threads.get_thread(thread).await?).is_ok());
        wait_completed(&threads, thread).await?;
        controller.drive(&id).await?;
        let committed = controller.store().await?.load_run(&id).await?;
        assert_eq!(
            committed.phase,
            RunPhase::Reporting,
            "the second read must recover terminal evidence"
        );
        assert_eq!(reads.load(Ordering::SeqCst), 2);
        assert_eq!(finished(&controller, &id).await["phase"], "completed");
        for _ in 0..3 {
            controller.tick().await?;
        }
        assert_eq!(server.received_requests().await.expect("requests").len(), 1);
        Ok(())
    })
}

#[test]
fn delayed_terminal_visibility_remains_reconcilable_live_shutdown_and_cold() -> anyhow::Result<()> {
    run_test_with_large_stack("audex-delayed-terminal-history", || async {
        for mode in ["live", "shutdown", "cold"] {
            let server = responses::start_mock_server().await;
            let temp = tempfile::tempdir()?;
            let hidden = Arc::new(AtomicBool::new(true));
            let hooks = || {
                let hidden = hidden.clone();
                DispatchTestHooks {
                    history: Some(Arc::new(move |mut history| {
                        if hidden.load(Ordering::SeqCst) {
                            hide_terminal(&mut history);
                        }
                        Box::pin(async move { Ok(history) })
                    })),
                    ..Default::default()
                }
            };
            let (original, threads) = with_hooks(&temp, &server.uri(), hooks()).await;
            let id = start(&original, "delayed-terminal").await;
            let run = original.store().await?.load_run(&id).await?;
            let raw = String::from_utf8(empty_result(&run))?;
            responses::mount_sse_once(
                &server,
                sse(vec![
                    ev_response_created("one"),
                    ev_assistant_message("answer", &raw),
                    ev_completed("one"),
                ]),
            )
            .await;
            original.drive(&id).await?;
            let thread = execution_thread(&original, &id).await;
            wait_completed(&threads, thread).await?;
            if mode != "live" {
                threads
                    .get_thread(thread)
                    .await?
                    .shutdown_and_wait()
                    .await?;
            }
            let (controller, cold_threads) = if mode == "cold" {
                expire(&original, &run.operation_id).await?;
                let (controller, threads) = with_hooks(&temp, &server.uri(), hooks()).await;
                (controller, Some(threads))
            } else {
                (original.clone(), None)
            };
            for _ in 0..3 {
                controller.drive(&id).await?;
                let retained = controller.store().await?.load_run(&id).await?;
                assert!(
                    retained.report.is_none(),
                    "status is not terminal evidence: {mode}"
                );
                assert!(controller.store().await?.raw_result(&retained)?.is_none());
            }
            hidden.store(false, Ordering::SeqCst);
            let completed = finished(&controller, &id).await;
            assert_eq!(
                completed["phase"], "completed",
                "late positive evidence must remain usable: {mode}"
            );
            assert_eq!(completed["rawResult"], raw);
            let revision = completed["revision"].clone();
            for _ in 0..3 {
                controller.tick().await?;
            }
            assert_eq!(read(&controller, &id).await["revision"], revision);
            assert_eq!(server.received_requests().await.expect("requests").len(), 1);
            if let Some(threads) = cold_threads {
                assert!(threads.list_thread_ids().await.is_empty());
            }
            threads
                .shutdown_all_threads_bounded(Duration::from_secs(5))
                .await;
        }
        Ok(())
    })
}

#[test]
fn failure_to_persist_an_observation_is_returned_without_replacing_the_operation()
-> anyhow::Result<()> {
    run_test_with_large_stack("audex-diagnostic-write-failure", || async {
        let server = responses::start_mock_server().await;
        let temp = tempfile::tempdir()?;
        let fail = Arc::new(AtomicBool::new(true));
        let injected = fail.clone();
        let hooks = DispatchTestHooks {
            history: Some(Arc::new(move |history| {
                let fail = injected.load(Ordering::SeqCst);
                Box::pin(async move {
                    if fail {
                        Err(ThreadStoreError::Internal {
                            message: "injected history failure".to_string(),
                        })
                    } else {
                        Ok(history)
                    }
                })
            })),
            ..Default::default()
        };
        let (controller, threads) = with_hooks(&temp, &server.uri(), hooks).await;
        let id = start(&controller, "diagnostic-write").await;
        let store = controller.store().await?;
        let run = store.load_run(&id).await?;
        let raw = String::from_utf8(empty_result(&run))?;
        responses::mount_sse_once(
            &server,
            sse(vec![
                ev_response_created("one"),
                ev_assistant_message("answer", &raw),
                ev_completed("one"),
            ]),
        )
        .await;
        controller.drive(&id).await?;
        wait_completed(&threads, execution_thread(&controller, &id).await).await?;
        let before = store.load_run(&id).await?;
        let pool = store.pool_for_tests();
        sqlx::query("CREATE TRIGGER unavailable_diagnostic BEFORE UPDATE OF error ON operations BEGIN SELECT RAISE(ABORT,'injected unavailable diagnostic storage'); END").execute(&pool).await?;
        assert!(
            controller.drive(&id).await.is_err(),
            "a failed diagnostic write must not be swallowed"
        );
        assert_eq!(store.load_run(&id).await?.revision, before.revision);
        assert_eq!(
            store.load_run(&id).await?.operation_state,
            OperationState::Accepted
        );
        assert!(issue(&controller, &run.operation_id).await?.0.is_none());
        sqlx::query("DROP TRIGGER unavailable_diagnostic")
            .execute(&pool)
            .await?;
        controller.drive(&id).await?;
        let text = issue(&controller, &run.operation_id)
            .await?
            .0
            .expect("diagnostic after storage recovers");
        assert!(text.contains("thread_store_internal"));
        fail.store(false, Ordering::SeqCst);
        assert_eq!(finished(&controller, &id).await["phase"], "completed");
        assert_eq!(server.received_requests().await.expect("requests").len(), 1);
        threads
            .shutdown_all_threads_bounded(Duration::from_secs(5))
            .await;
        Ok(())
    })
}
