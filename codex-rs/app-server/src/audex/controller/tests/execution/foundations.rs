//! Verify that the foundation uses native isolated Codex sessions, not a fake-only path.
use super::*;
use wiremock::matchers::body_string_contains;

#[test]
fn native_roster_uses_three_distinct_durable_sessions_without_sibling_input() -> anyhow::Result<()>
{
    run_test_with_large_stack("audex-native-roster", || async {
        let server = responses::start_mock_server().await;
        let temp = tempfile::tempdir()?;
        let (controller, threads) = real_fixture(&temp, &server.uri()).await;
        let id = super::super::foundations::roster(&controller, "native-roster", 3).await?;
        let store = controller.store().await?;
        let attempts = store.attempts(&id).await?;
        for (ordinal, attempt) in attempts.iter().enumerate() {
            let raw = json!({"schema_version":1,"snapshot_id":attempt.snapshot_id,
                "assignment_id":attempt.assignment_id,"summary":format!("PRIVATE_REVIEW_{ordinal}"),"findings":[]}).to_string();
            Mock::given(method("POST"))
                .and(path("/v1/responses"))
                .and(body_string_contains(attempt.assignment_id.clone()))
                .respond_with(
                    ResponseTemplate::new(200)
                        .insert_header("content-type", "text/event-stream")
                        .set_body_string(sse(vec![
                            ev_response_created(&format!("response-{ordinal}")),
                            ev_assistant_message(&format!("answer-{ordinal}"), &raw),
                            ev_completed(&format!("response-{ordinal}")),
                        ])),
                )
                .expect(1)
                .mount(&server)
                .await;
        }
        controller.tick().await?;
        let mut sessions = std::collections::HashSet::new();
        for attempt in &attempts {
            let (thread, turn, policy) = store
                .execution(&attempt.operation_id)
                .await?
                .expect("reserved native session");
            assert!(sessions.insert(thread.clone()), "fresh runtime per attempt");
            assert!(turn.is_some());
            assert_eq!(policy, store.attempt_input(&attempt.operation_id).await?.2);
            super::recovery::wait_completed(&threads, ThreadId::from_string(&thread)?).await?;
        }
        controller.tick().await?;
        let complete = store.attempts(&id).await?;
        assert!(
            complete
                .iter()
                .all(|a| a.assignment_state == AssignmentState::Succeeded)
        );
        let requests = server
            .received_requests()
            .await
            .expect("recorded native requests");
        assert_eq!(requests.len(), 3);
        for request in &requests {
            let body: Value = request.body_json()?;
            let text = body.to_string();
            let attempt = attempts
                .iter()
                .find(|a| text.contains(&a.assignment_id))
                .expect("exact native assignment");
            assert!(text.contains(&attempt.snapshot_id));
            assert_eq!(body["model"], attempt.model);
            assert!(!text.contains("PRIVATE_REVIEW_"));
            for peer in attempts
                .iter()
                .filter(|p| p.assignment_id != attempt.assignment_id)
            {
                assert!(!text.contains(&peer.assignment_id));
            }
        }
        // Cold reconstruction retains distinct native identities and results.
        let reopened = AuditStore::open(controller.root.clone()).await?;
        assert_eq!(reopened.attempts(&id).await?, complete);
        for attempt in &attempts {
            assert_eq!(
                reopened.execution(&attempt.operation_id).await?,
                store.execution(&attempt.operation_id).await?
            );
        }
        for thread in sessions {
            threads
                .get_thread(ThreadId::from_string(&thread)?)
                .await?
                .submit(codex_protocol::protocol::Op::Shutdown)
                .await?;
        }
        Ok(())
    })
}
