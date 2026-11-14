//! Public roster commissioning and the existing bounded/attempt authorities.
use super::*;

fn request(controller: &AuditController, key: &str, count: usize, sequential: bool) -> Value {
    json!({"requestKey":key,"source":{"type":"snippet","name":"example.rs","content":"fn main() {}\n"},
        "brief":"Static correctness audit","audexGuidance":"EXPLICIT_GUIDANCE","agentsEvidence":"EXPLICIT_EVIDENCE",
        "scheduling":if sequential {"sequential"} else {"automatic"},
        "reviewers":(0..count).map(|_|json!({"provider":controller.config.model_provider_id,"model":"audit-test"})).collect::<Vec<_>>()})
}

#[tokio::test]
async fn public_roster_is_atomic_ordered_and_idempotent_even_after_lost_acknowledgement()
-> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let (controller, fake, _) = fixture(&temp).await;
    let params = request(&controller, "public-roster", 3, false);
    controller.fail_start_response.store(true, Ordering::SeqCst);
    let error = api(&controller, "audit/start", params.clone())
        .await
        .expect_err("lost acknowledgement");
    let id = error.data.expect("durable recovery")["runId"]
        .as_str()
        .expect("run")
        .to_owned();
    controller
        .fail_start_response
        .store(false, Ordering::SeqCst);
    let replay = api(&controller, "audit/start", params.clone())
        .await
        .expect("idempotent replay");
    assert_eq!(replay["run"]["runId"], id);
    let mut explicit_native = params.clone();
    for selection in explicit_native["reviewers"].as_array_mut().unwrap() {
        selection["executionAdapter"] = json!("native");
        selection["authenticationRoute"] = json!("daemon_configured_native");
    }
    let replay = api(&controller, "audit/start", explicit_native)
        .await
        .expect("native defaults retain identity");
    assert_eq!(replay["run"]["runId"], id);
    let attempts = controller.store().await?.attempts(&id).await?;
    assert_eq!(attempts.len(), 3);
    for (ordinal, a) in attempts.iter().enumerate() {
        assert_eq!(a.reviewer_ordinal, ordinal as i64);
        assert_eq!(a.snapshot_id, attempts[0].snapshot_id);
        assert_eq!(a.model, "audit-test");
        assert_eq!(a.attempt_ordinal, 1);
        let input: Value = serde_json::from_str(
            &controller
                .store()
                .await?
                .attempt_input(&a.operation_id)
                .await?
                .0,
        )?;
        assert_eq!(input["requireServedIdentity"], true);
    }
    for identity in [
        attempts
            .iter()
            .map(|a| &a.reviewer_id)
            .collect::<HashSet<_>>(),
        attempts.iter().map(|a| &a.assignment_id).collect(),
        attempts.iter().map(|a| &a.operation_id).collect(),
    ] {
        assert_eq!(identity.len(), 3);
    }
    for changed in [
        {
            let mut p = params.clone();
            p["reviewers"][2]["model"] = json!("changed");
            p
        },
        {
            let mut p = params.clone();
            p["scheduling"] = json!("sequential");
            p
        },
        {
            let mut p = params.clone();
            p["reviewers"][0]["expectedServedModel"] = json!("pinned-alias");
            p
        },
    ] {
        assert!(api(&controller, "audit/start", changed).await.is_err());
    }
    let summary = read(&controller, &id).await;
    assert_eq!(summary["collection"]["reviewerCount"], 3);
    assert_eq!(summary["collection"]["outstandingCount"], 3);
    assert_eq!(summary["reviewers"].as_array().expect("roster").len(), 3);
    assert_eq!(
        fake.starts.load(Ordering::SeqCst),
        0,
        "commission/read does not dispatch inference"
    );
    let mut invalid = request(&controller, "bad-preflight", 3, false);
    invalid["reviewers"][2]["provider"] = json!("unsupported-provider");
    assert!(api(&controller, "audit/start", invalid).await.is_err());
    assert_eq!(
        controller.store().await?.list_run_ids("", 10).await?,
        vec![id]
    );
    assert_eq!(fake.starts.load(Ordering::SeqCst), 0);
    for reviewers in [json!([]), json!([{"provider":"","model":"audit-test"}])] {
        let mut invalid = params.clone();
        invalid["requestKey"] = json!("invalid");
        invalid["reviewers"] = reviewers;
        assert!(api(&controller, "audit/start", invalid).await.is_err());
    }
    Ok(())
}

#[tokio::test]
async fn automatic_and_sequential_rosters_queue_without_changing_logical_order()
-> anyhow::Result<()> {
    for sequential in [false, true] {
        let temp = tempfile::tempdir()?;
        let (original, fake, _) = fixture(&temp).await;
        let controller = AuditController::new(
            original.config.clone(),
            original.models.clone(),
            fake.clone(),
            Arc::new(|_| {}),
        );
        let started = api(
            &controller,
            "audit/start",
            request(&controller, "queue", 4, sequential),
        )
        .await
        .expect("commission");
        let id = started["run"]["runId"].as_str().expect("id");
        controller.tick().await?;
        let records = controller.store().await?.attempts(id).await?;
        let capacity = if sequential { 1 } else { 3 };
        assert_eq!(fake.starts.load(Ordering::SeqCst), capacity);
        assert_eq!(
            records
                .iter()
                .filter(|r| r.operation_state == OperationState::Accepted)
                .count(),
            capacity
        );
        for (ordinal, attempt) in records.iter().enumerate() {
            if sequential || ordinal >= capacity {
                for _ in 0..3 {
                    controller.tick().await?;
                }
                assert_eq!(
                    fake.starts.load(Ordering::SeqCst),
                    if sequential { ordinal + 1 } else { 4 }
                );
            }
            fake.replace(
                &attempt.operation_id,
                WorkState::Completed(empty_result(attempt)),
            );
            controller.drive_operation(&attempt.operation_id).await?;
        }
        controller.tick().await?;
        let view = read(&controller, id).await;
        assert_eq!(view["phase"], "completed");
        assert_eq!(view["collection"]["completedCount"], 4);
        assert_eq!(view["collection"]["ready"], true);
        assert_eq!(fake.starts.load(Ordering::SeqCst), 4);
        let after = controller.store().await?.attempts(id).await?;
        assert_eq!(
            records.iter().map(|r| &r.assignment_id).collect::<Vec<_>>(),
            after.iter().map(|r| &r.assignment_id).collect::<Vec<_>>()
        );
    }
    Ok(())
}

#[tokio::test]
async fn blocked_sequential_peers_do_not_reserve_other_audits_worker_slots() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let (original, fake, _) = fixture(&temp).await;
    let controller = AuditController::new(
        original.config.clone(),
        original.models.clone(),
        fake.clone(),
        Arc::new(|_| {}),
    );
    api(
        &controller,
        "audit/start",
        request(&controller, "sequential-first", 4, true),
    )
    .await
    .expect("first");
    controller.tick().await?;
    assert_eq!(fake.starts.load(Ordering::SeqCst), 1);
    api(
        &controller,
        "audit/start",
        request(&controller, "automatic-second", 2, false),
    )
    .await
    .expect("second");
    controller.tick().await?;
    assert_eq!(
        fake.starts.load(Ordering::SeqCst),
        3,
        "one sequential reviewer and two independent peers"
    );
    Ok(())
}

#[tokio::test]
async fn model_disqualification_is_durable_and_fences_generations_without_discarding_capture()
-> anyhow::Result<()> {
    use crate::audex::store::activity::ActivityInput;
    use crate::audex::store::admission::AdmissionIdentity;
    use crate::audex::store::attempts::AttemptAction;
    use crate::audex::store::attempts::ControlRequest;
    for mode in ["missing", "mismatch", "connection_only"] {
        let (_temp, controller, fake, id, attempts) = super::attempts::setup().await?;
        let store = controller.store().await?;
        let capture = super::attempts::capture(&controller, &fake, &attempts[0]).await?;
        let attempt = &attempts[0];
        let identity = AdmissionIdentity {
            run_id: id.clone(),
            assignment_id: attempt.assignment_id.clone(),
            operation_id: attempt.operation_id.clone(),
            execution_id: capture.identity().session_id.clone(),
            owner: controller.owner.clone(),
            owner_generation: attempt.owner_generation,
            run_stop_generation: store.run_stop_generation(&id).await?,
        };
        let now = chrono::Utc::now().timestamp_millis();
        store
            .admit_generation(&identity, "physical-first", "native_http", now)
            .await?;
        assert!(
            store
                .mark_generation_send(&identity, "physical-first", now)
                .await?
        );
        let disqualified = if mode != "mismatch" {
            if mode == "connection_only" {
                store
                    .check_connection_model(&identity, "physical-first", &attempt.model)
                    .await?;
                assert!(
                    store
                        .admit_generation(&identity, "not-yet-qualified", "native_websocket", now)
                        .await
                        .is_ok()
                );
            }
            store
                .attribute_generation(&identity, "physical-first", None, true)
                .await
        } else {
            store
                .attribute_generation(&identity, "physical-first", Some("unexpected-model"), false)
                .await
        };
        assert!(disqualified.is_err());
        let reopened = AuditStore::open(controller.root.clone()).await?;
        assert!(
            reopened
                .admit_generation(&identity, "forbidden-continuation", "native_websocket", now)
                .await
                .is_err()
        );
        // Already received public material remains eligible history, never a validated result.
        store
            .append_activity(
                capture.identity(),
                &[ActivityInput {
                    producer_key: "received-prefix".into(),
                    turn_id: "turn".into(),
                    item_id: "message".into(),
                    kind: "assistant_delta".into(),
                    payload: "UNVALIDATED_RECEIVED_PREFIX".into(),
                    text_offset: Some(0),
                }],
            )
            .await?;
        let current = store
            .attempt_by_assignment(&id, &attempt.assignment_id)
            .await?;
        let snapshot = controller
            .snapshots()
            .open(&SnapshotId::parse(attempt.snapshot_id.clone())?)
            .await?;
        assert!(
            store
                .commit_seeded_result(
                    &attempt.operation_id,
                    current.attempt_revision,
                    &snapshot,
                    &empty_result(attempt)
                )
                .await
                .is_err()
        );
        store
            .control_attempt(
                &ControlRequest {
                    run_id: id,
                    assignment_id: attempt.assignment_id.clone(),
                    action_key: "exclude-disqualified".into(),
                    action: AttemptAction::Ignore,
                    reason: "invalid attribution".into(),
                    authorise_retry: false,
                    acknowledge_possible_overlap: false,
                },
                None,
            )
            .await?;
        assert_eq!(
            store.activity(&attempt.assignment_id, 0, 64).await?[0].payload,
            "UNVALIDATED_RECEIVED_PREFIX"
        );
        let evidence = reopened.model_evidence(&attempt.assignment_id).await?;
        assert_eq!(
            evidence.len(),
            if mode == "connection_only" { 2 } else { 1 }
        );
        if mode == "connection_only" {
            assert!(
                !reopened
                    .mark_generation_send(&identity, "not-yet-qualified", now)
                    .await?
            );
            assert_eq!(
                evidence
                    .iter()
                    .find(|e| e.request_id == "not-yet-qualified")
                    .unwrap()
                    .state,
                "pending"
            );
        }
        assert_eq!(
            evidence
                .iter()
                .find(|e| e.request_id == "physical-first")
                .unwrap()
                .state,
            if mode != "mismatch" {
                "missing"
            } else {
                "mismatch"
            }
        );
        assert_eq!(
            fake.starts.load(Ordering::SeqCst),
            3,
            "no implicit replacement inference"
        );
    }
    Ok(())
}

#[tokio::test]
async fn typed_route_preflight_rejects_whole_roster_before_capture_or_dispatch()
-> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let (controller, fake, notifications) = fixture(&temp).await;
    for (executor, route, message) in [
        ("claude_client", "claude_subscription", "unavailable"),
        ("claude_client", "claude_api", "unavailable"),
        ("native", "ollama_local", "unavailable"),
        ("native", "claude_subscription", "incompatible"),
        ("native", "claude_api", "incompatible"),
        ("claude_client", "daemon_configured_native", "incompatible"),
        ("claude_client", "ollama_local", "incompatible"),
    ] {
        for position in 0..3 {
            let mut params = request(&controller, "rejected", 3, false);
            params["reviewers"][position]["executionAdapter"] = json!(executor);
            params["reviewers"][position]["authenticationRoute"] = json!(route);
            let error = api(&controller, "audit/start", params).await.unwrap_err();
            assert!(error.message.contains(message), "{error:?}");
            assert!(
                controller.store.get().is_none(),
                "preflight must not open the store"
            );
            assert!(
                !controller.root.exists(),
                "no captured source or durable state"
            );
            assert_eq!(fake.starts.load(Ordering::SeqCst), 0);
            assert!(notifications.lock().unwrap().is_empty());
        }
    }
    // An explicit scheduler tick must still find no valid preceding member to launch.
    controller.tick().await?;
    assert!(
        controller
            .store()
            .await?
            .list_run_ids("", 100)
            .await?
            .is_empty()
    );
    assert_eq!(fake.starts.load(Ordering::SeqCst), 0);
    Ok(())
}
