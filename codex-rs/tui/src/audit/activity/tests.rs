use super::*;
use codex_app_server_protocol::AuditActivityEntry;

fn viewer() -> ActivityViewer {
    ActivityViewer::new(
        ThreadId::new(),
        crate::audit::tests::attempt_fixture(),
        &crate::keymap::RuntimeKeymap::defaults(),
    )
}
fn entry(sequence: i64, kind: &str, payload: &str) -> AuditActivityEntry {
    AuditActivityEntry {
        sequence,
        session_id: "session".into(),
        turn_id: "turn".into(),
        item_id: format!("item-{sequence}"),
        kind: kind.into(),
        payload: payload.into(),
        payload_digest: "retained-digest".into(),
    }
}
fn page(target: &AuditAttemptView, entries: Vec<AuditActivityEntry>) -> AuditAttemptReadResponse {
    AuditAttemptReadResponse {
        attempt: target.clone(),
        next_sequence: entries.last().map_or(0, |e| e.sequence),
        entries,
        raw_result: Some("DO_NOT_RENDER_RAW_RESULT".into()),
    }
}
fn text(viewer: &mut ActivityViewer) -> String {
    let area = Rect::new(0, 0, 220, 30);
    let mut buf = Buffer::empty(area);
    viewer.render(area, &mut buf);
    buf.content()
        .chunks(220)
        .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn current_and_historical_activity_remains_attributed_and_untrusted() {
    for state in [
        "active",
        "cancelled",
        "ignored",
        "superseded",
        "failed",
        "completed",
    ] {
        let mut view = viewer();
        view.target.disposition = state.into();
        view.target.current = state == "active";
        view.target.completed = state == "completed";
        view.apply(Ok(page(
            &view.target,
            vec![
                entry(1, "assistant_delta", "PUBLIC_PREFIX"),
                entry(2, "tool_result", "source.rs"),
                entry(3, "status", "turn_completed"),
            ],
        )));
        let rendered = text(&mut view);
        for expected in [
            "reviewer-1",
            "native-model",
            "assignment-original",
            state,
            "PUBLIC_PREFIX",
            "Source/tool activity",
            "Execution status",
            "unvalidated",
        ] {
            assert!(rendered.contains(expected), "{state}: {expected}");
        }
        assert!(!rendered.contains("DO_NOT_RENDER_RAW_RESULT"));
        assert!(!rendered.contains("browse prompts"));
        if !view.target.current {
            assert!(
                view.action_for_key(KeyEvent::new(KeyCode::Char('R'), KeyModifiers::NONE))
                    .is_none()
            );
        }
    }
}

#[test]
fn completed_unstructured_opinion_is_literal_and_not_duplicated_on_reconnect() {
    let mut view = viewer();
    view.target.completed = true;
    view.target.extraction = Some(codex_app_server_protocol::AuditResultExtraction {
        version: 1,
        raw_digest: "retained-opinion-digest".into(),
        parse_digest: "retained-opinion-digest".into(),
        status: codex_app_server_protocol::AuditExtractionStatus::UnstructuredOpinion,
        transformations: vec![],
        validation_error: Some("result assignment ID does not match the assignment".into()),
    });
    let mut response = page(&view.target, vec![]);
    response.raw_result = Some("ORIGINAL_OPINION <script>not executable</script>\u{1b}[31m".into());
    view.apply(Ok(response.clone()));
    view.apply(Ok(response));
    let rendered = text(&mut view);
    assert!(rendered.contains("Completed opinion"));
    assert!(rendered.contains("no validated"));
    assert!(rendered.contains("result assignment ID does not match"));
    assert_eq!(rendered.matches("ORIGINAL_OPINION").count(), 1);
    assert!(rendered.contains("<script>not executable</script>"));
    assert!(!rendered.contains('\u{1b}'));
    assert!(
        view.action_for_key(KeyEvent::new(KeyCode::Char('R'), KeyModifiers::NONE))
            .is_none()
    );
}

#[test]
fn tool_only_gap_truncation_quiet_and_disconnection_are_distinct() {
    let mut view = viewer();
    assert!(text(&mut view).contains("No observable committed activity"));
    view.target.capture_gap = Some("retained capture gap".into());
    view.target.capture_truncated = true;
    view.apply(Ok(page(
        &view.target,
        vec![
            entry(1, "tool_start", "read source"),
            entry(2, "gap", "missing tail"),
            entry(3, "truncation", "size bound"),
        ],
    )));
    let rendered = text(&mut view);
    for expected in [
        "Connected",
        "No review output available",
        "CAPTURE GAP",
        "CAPTURE TRUNCATED",
        "truncated=true",
    ] {
        assert!(rendered.contains(expected), "{expected}");
    }
    view.apply(Err("secret provider error must not render".into()));
    let rendered = text(&mut view);
    assert!(rendered.contains("Disconnected / reconnecting"));
    assert!(!rendered.contains("secret provider"));
}

#[test]
fn duplicate_pages_cutoffs_and_private_payloads_cannot_extend_public_text() {
    let mut view = viewer();
    view.target.output_cutoff = Some(2);
    let entries = vec![
        entry(1, "assistant_delta", "ACCEPTED_PREFIX\x1b[31m"),
        entry(2, "private_reasoning", "PRIVATE_SECRET"),
        entry(3, "assistant_delta", "AFTER_CUTOFF"),
    ];
    view.apply(Ok(page(&view.target, entries.clone())));
    view.apply(Ok(page(&view.target, entries)));
    assert_eq!(view.cursor, 2);
    let rendered = text(&mut view);
    assert_eq!(rendered.matches("ACCEPTED_PREFIX").count(), 1);
    for absent in [
        "PRIVATE_SECRET",
        "AFTER_CUTOFF",
        "\x1b",
        "DO_NOT_RENDER_RAW_RESULT",
    ] {
        assert!(!rendered.contains(absent));
    }
    assert!(rendered.contains("Unsupported activity withheld"));
}

#[tokio::test]
async fn append_preserves_reading_position_and_end_resumes_durable_tail() {
    let mut view = viewer();
    view.target.output_sequence = 100;
    view.apply(Ok(page(
        &view.target,
        (1..=70)
            .map(|i| entry(i, "assistant_delta", &format!("row-{i}")))
            .collect(),
    )));
    text(&mut view);
    view.transcript.scroll(-45);
    let before = text(&mut view);
    assert!(!view.transcript.is_scrolled_to_bottom());
    view.apply(Ok(page(
        &view.target,
        (71..=100)
            .map(|i| entry(i, "assistant_delta", &format!("row-{i}")))
            .collect(),
    )));
    let after = text(&mut view);
    // Identity/sequence and scrollbar can change; the visible review rows remain anchored.
    let rows = |s: &str| {
        s.lines()
            .filter(|line| line.starts_with("row-"))
            .map(str::trim_end)
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    assert_eq!(rows(&before), rows(&after));
    assert!(!rows(&after).is_empty());
    let mut tui = crate::tui::test_support::make_test_tui().unwrap();
    view.transcript
        .handle_event(
            &mut tui,
            crate::tui::TuiEvent::Key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE)),
        )
        .unwrap();
    let latest = text(&mut view);
    assert!(view.transcript.is_scrolled_to_bottom());
    assert!(latest.contains("row-100"));
}

#[test]
fn contextual_actions_respect_modes_readiness_and_permanent_ignore() {
    use codex_app_server_protocol::AuditAttemptAction::*;
    let mut view = viewer();
    for (key, action) in [('C', Cancel), ('R', Retry), ('I', Ignore)] {
        assert_eq!(
            view.action_for_key(KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE)),
            Some(action)
        );
    }
    for state in ["accepted", "failed", "unknown"] {
        view.target.operation_state = state.into();
        assert_eq!(
            view.action_for_key(KeyEvent::new(KeyCode::Char('R'), KeyModifiers::NONE)),
            Some(Retry)
        );
    }
    view.target.disposition = "cancelled".into();
    assert_eq!(
        view.action_for_key(KeyEvent::new(KeyCode::Char('R'), KeyModifiers::NONE)),
        Some(Retry)
    );
    view.target.retry_eligible = false;
    assert!(
        view.action_for_key(KeyEvent::new(KeyCode::Char('R'), KeyModifiers::NONE))
            .is_none()
    );
    view.target.disposition = "ignored".into();
    view.target.retry_eligible = true;
    assert!(
        view.action_for_key(KeyEvent::new(KeyCode::Char('R'), KeyModifiers::NONE))
            .is_none()
    );
    view.target.disposition = "active".into();
    view.transcript.begin_search();
    assert!(
        view.action_for_key(KeyEvent::new(KeyCode::Char('C'), KeyModifiers::NONE))
            .is_none()
    );
}

#[tokio::test]
async fn paging_reconnect_and_missed_notifications_replay_only_durable_cursors() {
    use codex_app_server_client::{AppServerClient, CloudClient, CloudOperation};
    let (cloud, mut commands, _incoming) = CloudClient::channel(8);
    let client = AppServerClient::Cloud(cloud);
    let (tx, mut events) = tokio::sync::mpsc::unbounded_channel();
    let sender = AppEventSender::new(tx);
    let mut view = viewer();
    view.target.output_sequence = 130;
    view.reconnect(client.request_handle(), sender.clone());
    for (after, end) in [(0, 128), (128, 130), (130, 131)] {
        // Third page arrives on polling alone: no notification and unchanged run/attempt revision.
        let command = tokio::time::timeout(Duration::from_secs(3), commands.recv())
            .await
            .unwrap()
            .unwrap();
        let CloudOperation::Request(request) = command.operation else {
            panic!("read request")
        };
        let ClientRequest::AuditAttemptRead { params, .. } = *request else {
            panic!("only attempt/read is allowed")
        };
        assert_eq!(params.assignment_id, "assignment-original");
        assert_eq!(params.after_sequence, Some(after));
        assert_eq!(params.limit, Some(PAGE_SIZE));
        view.target.output_sequence = end.max(130);
        let response = page(
            &view.target,
            ((after + 1)..=end)
                .map(|i| entry(i, "assistant_delta", &format!("entry-{i}")))
                .collect(),
        );
        command
            .response
            .send(Ok(Ok(serde_json::to_value(response).unwrap())))
            .unwrap();
        let AppEvent::AuditActivityPage {
            viewer_id,
            page,
            consumed,
            ..
        } = events.recv().await.unwrap()
        else {
            panic!("page")
        };
        assert_eq!(viewer_id, view.id);
        view.apply(*page);
        assert!(
            commands.try_recv().is_err(),
            "one unconsumed page bounds the reader"
        );
        consumed.send(()).unwrap();
    }
    assert_eq!(view.cursor, 131);
    // Rebind a replacement transport from the consumed cursor, preserving exact assignment.
    let (replacement, mut new_commands, _incoming2) = CloudClient::channel(8);
    let new_client = AppServerClient::Cloud(replacement);
    view.reconnect(new_client.request_handle(), sender);
    let command = new_commands.recv().await.unwrap();
    assert!(
        matches!(command.operation,CloudOperation::Request(r) if matches!(&*r,ClientRequest::AuditAttemptRead{params,..} if params.after_sequence==Some(131) && params.assignment_id=="assignment-original"))
    );
    drop(view);
    assert!(
        new_commands.try_recv().is_err(),
        "closing only aborts observation"
    );
}
