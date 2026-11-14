//! Native completed history plus an unfinished streamed item after execution failure.
use super::*;
use codex_protocol::models::{ContentItem, ResponseItem};
use codex_protocol::protocol::ThreadHistoryMode;
use codex_rollout::RolloutItem;
use codex_thread_store::{ItemSortKey, ListItemsParams, LoadThreadHistoryParams, SortDirection};

const PROGRESS: &str = "Completed progress: first range checked.";
const TAIL: &str = "The second message is unfinished";

#[test]
fn native_paginated_recovery_preserves_unfinished_streamed_tail() -> anyhow::Result<()> {
    run_test_with_large_stack("m5-paginated-tail", || exercise(false, false))
}

#[test]
fn native_legacy_recovery_preserves_unfinished_streamed_tail() -> anyhow::Result<()> {
    run_test_with_large_stack("m5-legacy-tail", || exercise(true, false))
}

#[test]
fn native_paginated_recovery_keeps_history_only_messages_with_identical_text() -> anyhow::Result<()>
{
    run_test_with_large_stack("m5-paginated-history-only", || exercise(false, true))
}

#[test]
fn native_legacy_recovery_keeps_history_only_messages_with_identical_text() -> anyhow::Result<()> {
    run_test_with_large_stack("m5-legacy-history-only", || exercise(true, true))
}

async fn exercise(legacy: bool, history_only: bool) -> anyhow::Result<()> {
    let server = responses::start_mock_server().await;
    let mut events = vec![
        ev_response_created("partial-review"),
        ev_assistant_message("progress", PROGRESS),
    ];
    if history_only {
        // A distinct completed item with identical text must not be text-deduplicated.
        events.push(ev_assistant_message("history-only", PROGRESS));
    }
    events.extend([
            responses::ev_message_item_added("unfinished", ""),
            responses::ev_output_text_delta("The sec"),
            responses::ev_output_text_delta("ond message is unfi"),
            responses::ev_output_text_delta("nished"),
            json!({"type":"response.failed","response":{"id":"partial-review",
                "error":{"code":"invalid_prompt","message":"Deterministic terminal stream failure"}}}),
    ]);
    let mock = responses::mount_sse_once(&server, sse(events)).await;
    let temp = tempfile::tempdir()?;
    let (controller, threads) = fixture_with_hooks(
        &temp,
        &server.uri(),
        CodexAuth::from_api_key("dummy"),
        false,
        if history_only {
            // Deliberately miss one completed-item capture notification. Native
            // persistence remains untouched, so that message exists only there.
            Arc::new(|_, thread| {
                Box::pin(async move {
                    tokio::spawn(async move {
                        while let Ok(event) = thread.next_event().await {
                            let omitted = matches!(&event.msg,
                            codex_protocol::protocol::EventMsg::ItemCompleted(completed)
                            if completed.item.id() == "history-only");
                            if !omitted
                                && let Some(capture) = thread
                                    .thread_extension_data()
                                    .get::<crate::audex::capture::CaptureHandle>(
                                )
                            {
                                capture.observe(&event).await;
                            }
                            if matches!(
                                event.msg,
                                codex_protocol::protocol::EventMsg::ShutdownComplete
                            ) {
                                break;
                            }
                        }
                    });
                    Ok(())
                })
            })
        } else {
            admission::native_reader()
        },
        DispatchTestHooks {
            history_mode: legacy.then_some(ThreadHistoryMode::Legacy),
            ..Default::default()
        },
    )
    .await;
    let id = start(&controller, "native-unfinished-tail").await;
    let store = controller.store().await?;
    let initial = store.load_run_metadata(&id).await?;
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            controller.drive_operation(&initial.operation_id).await?;
            if store.load_run_metadata(&id).await?.assignment_state == AssignmentState::Failed {
                break anyhow::Ok(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    // Re-observe the original after failure, when the history projection is retained.
    controller.drive_operation(&initial.operation_id).await?;
    threads
        .shutdown_all_threads_bounded(Duration::from_secs(5))
        .await;
    let (thread, turn, _) = store.execution(&initial.operation_id).await?.unwrap();
    let thread_id = ThreadId::from_string(&thread)?;
    let state = codex_state::StateRuntime::init(
        controller.config.sqlite_config().clone(),
        controller.config.model_provider_id.clone(),
    )
    .await?;
    let history = ThreadStoreBuilder::from_config(&controller.config, Some(state)).build();
    let persisted = history
        .load_latest_model_context(LoadThreadHistoryParams {
            thread_id,
            include_archived: false,
        })
        .await?;
    let messages: Vec<_> = persisted
        .items
        .iter()
        .filter_map(|item| {
            if let RolloutItem::ResponseItem(envelope) = item
                && let ResponseItem::Message {
                    id, role, content, ..
                } = &envelope.item
                && role == "assistant"
            {
                let text = content
                    .iter()
                    .filter_map(|part| match part {
                        ContentItem::OutputText { text, .. } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<String>();
                Some(json!({"id":id,"text":text}))
            } else {
                None
            }
        })
        .collect();
    let display = if legacy {
        None
    } else {
        Some(
            history
                .list_items(ListItemsParams {
                    thread_id,
                    turn_id: turn,
                    item_ids: None,
                    include_archived: false,
                    position: None,
                    page_size: 256,
                    sort_direction: SortDirection::Asc,
                    sort_key: ItemSortKey::CreatedAtOrdinal,
                    after_updated_at_ordinal: None,
                    before_updated_at_ordinal: None,
                })
                .await?
                .items
                .into_iter()
                .map(|item| serde_json::from_slice::<Value>(&item.item_json))
                .collect::<Result<Vec<_>, _>>()?,
        )
    };
    let activity = store.activity(&initial.assignment_id, 0, 256).await?;
    let read = controller
        .read_attempt(AuditAttemptReadParams {
            run_id: id.clone(),
            assignment_id: initial.assignment_id.clone(),
            after_sequence: None,
            limit: Some(256),
        })
        .await?;
    let mode = format!(
        "{}{}",
        if legacy { "legacy" } else { "paginated" },
        if history_only { "-history-only" } else { "" }
    );
    let evidence = json!({"mode":mode,"nativeMessages":messages,"displayItems":display,
        "activity":activity,"operatorRecoveredOutput":read.recovered_output});
    if let Ok(path) = std::env::var("AUDEX_M5_NATIVE_EVIDENCE") {
        std::fs::create_dir_all(&path)?;
        std::fs::write(
            std::path::Path::new(&path).join(format!("{mode}.json")),
            serde_json::to_vec_pretty(&evidence)?,
        )?;
    }
    assert!(messages.iter().any(|item| item["text"] == PROGRESS));
    assert!(
        !messages.iter().any(|item| item["text"]
            .as_str()
            .is_some_and(|text| text.contains(TAIL))),
        "unfinished text must be checked against actual native persistence"
    );
    if let Some(display) = display {
        assert!(display.iter().any(|item| item["text"] == PROGRESS));
        assert!(!display.iter().any(|item| {
            item["text"]
                .as_str()
                .is_some_and(|text| text.contains(TAIL))
        }));
    }
    let deltas = activity
        .iter()
        .filter(|entry| entry.kind == "assistant_delta")
        .map(|entry| entry.payload.as_str())
        .collect::<String>();
    assert_eq!(deltas, TAIL, "the journal retains the unfinished stream");
    assert!(
        activity
            .iter()
            .any(|entry| entry.kind == "assistant_message" && entry.payload == PROGRESS)
    );
    assert_eq!(mock.requests().len(), 1, "no additional provider request");
    assert_eq!(store.attempts(&id).await?.len(), 1);
    assert!(!store.recovery_ready(&initial.assignment_id).await?);
    assert_eq!(
        store
            .recovery_status(&initial.assignment_id)
            .await?
            .as_deref(),
        Some("operator_retry_required_surviving_output")
    );
    let expected = if history_only {
        assert_eq!(
            messages
                .iter()
                .filter(|item| item["text"] == PROGRESS)
                .count(),
            2
        );
        assert!(!activity.iter().any(|entry| entry.item_id == "history-only"));
        format!("{PROGRESS}\n\n{PROGRESS}\n\n{TAIL}")
    } else {
        format!("{PROGRESS}\n\n{TAIL}")
    };
    assert_eq!(read.recovered_output.as_deref(), Some(expected.as_str()));
    let attribution = store.model_evidence(&initial.assignment_id).await?;
    let admissions = store.generation_records(&initial.assignment_id).await?;
    assert_eq!(admissions.len(), 1);
    controller.drive_operation(&initial.operation_id).await?;
    assert_eq!(
        serde_json::to_value(store.model_evidence(&initial.assignment_id).await?)?,
        serde_json::to_value(attribution)?
    );
    assert_eq!(
        store.generation_records(&initial.assignment_id).await?,
        admissions
    );
    assert_eq!(store.list_run_ids("", 100).await?.len(), 1);
    let reopened = AuditStore::open(controller.root.clone()).await?;
    assert_eq!(
        reopened
            .recovered_output(&initial.operation_id)
            .await?
            .as_deref(),
        Some(expected.as_str())
    );
    Ok(())
}
