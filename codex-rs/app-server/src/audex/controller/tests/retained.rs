//! C5.2 contract integration: real store transactions, sealed inputs and seeded evidence.
use super::*;
use crate::audex::backends::BackendSettings;
use crate::audex::retained::RetainedInput;
use crate::audex::store::admission::AdmissionIdentity;

fn request(controller: &AuditController, key: &str) -> Value {
    json!({"requestKey":key,"source":{"type":"snippet","name":"example.rs","content":"fn main() {}\n"},
        "brief":"Static correctness audit","provider":controller.config.model_provider_id,"model":"audit-test"})
}

fn retry(run: &str, assignment: &str, key: &str) -> Value {
    json!({"runId":run,"assignmentId":assignment,"actionKey":key,"action":"retry","reason":"approved fixture Retry",
        "authoriseRetry":true,"acknowledgePossibleOverlap":true})
}

fn configured_settings() -> Value {
    json!({"version":1,
        "claudeSubscription":{"executable":"/absent/claude","clientVersion":"2.1.314","profile":"/absent/profile","runtimeRoot":"/absent/private"},
        "claudeApi":{"executable":"/absent/claude","clientVersion":"2.1.314","runtimeRoot":"/absent/api","credential":{"kind":"file","reference":"/absent/credentials"},"upstream":"https://api.example.test"},
        "ollamaLocal":{"endpoint":"http://127.0.0.1:11434","resourceTag":"fixture","model":"fixture","modelDigest":"digest-original","contextTokens":8192,"outputTokens":1024}})
}

#[tokio::test]
async fn singular_idempotency_binds_submitted_selections_and_replays_original_approval()
-> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let (controller, fake, _) = fixture(&temp).await;
    let params = request(&controller, "singular-contract");
    let started = api(&controller, "audit/start", params.clone())
        .await
        .expect("start");
    let id = started["run"]["runId"].as_str().unwrap();
    let replay = api(&controller, "audit/start", params.clone())
        .await
        .expect("same action");
    assert_eq!(replay["run"]["runId"], id);
    let mut roster = params.clone();
    roster.as_object_mut().unwrap().remove("provider");
    roster.as_object_mut().unwrap().remove("model");
    roster["reviewers"] = json!([{"provider":controller.config.model_provider_id,"model":"audit-test","executionAdapter":"native","authenticationRoute":"daemon_configured_native"}]);
    assert_eq!(
        api(&controller, "audit/start", roster)
            .await
            .expect("equivalent native form")["run"]["runId"],
        id
    );
    for (field, value) in [
        ("expectedServedModel", json!("different")),
        ("scheduling", json!("sequential")),
        ("model", json!("different")),
        ("reasoningEffort", json!("high")),
        ("provider", json!("different")),
        ("brief", json!("different")),
        ("audexGuidance", json!("different")),
        ("agentsEvidence", json!("different")),
    ] {
        let mut changed = params.clone();
        changed[field] = value;
        let error = api(&controller, "audit/start", changed)
            .await
            .expect_err(field);
        assert_eq!(
            error.data.unwrap()["reason"],
            "auditIdempotencyConflict",
            "{field}"
        );
    }
    let settings_path = controller.root.join("reviewer-backends.json");
    std::fs::write(&settings_path, serde_json::to_vec(&configured_settings())?)?;
    for settings in [
        serde_json::to_vec(&configured_settings())?,
        b"{\"version\":999}".to_vec(),
    ] {
        std::fs::write(&settings_path, settings)?;
        let replay = api(&controller, "audit/start", params.clone())
            .await
            .expect("lookup retains original approval");
        assert_eq!(replay["run"]["runId"], id);
    }
    assert_eq!(
        controller.store().await?.list_run_ids("", 10).await?,
        vec![id.to_owned()]
    );
    assert_eq!(fake.starts.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test]
async fn retry_preserves_approved_bytes_and_source_but_revalidates_configuration()
-> anyhow::Result<()> {
    let (_temp, controller, fake, id, attempts) = super::attempts::setup().await?;
    let store = controller.store().await?;
    let old = &attempts[0];
    let capture = super::attempts::capture(&controller, &fake, old).await?;
    let retained = store.attempt_input(&old.operation_id).await?;
    let settings_path = controller.root.join("reviewer-backends.json");
    std::fs::write(&settings_path, b"{\"version\":999}")?;
    assert!(
        api(
            &controller,
            "audit/attempt/control",
            retry(&id, &old.assignment_id, "drift")
        )
        .await
        .is_err()
    );
    assert_eq!(store.attempts(&id).await?.len(), 3);
    assert_eq!(
        store.capture_state(&old.assignment_id).await?.disposition,
        "active"
    );
    // Valid changes to all external sections leave this native approval valid.
    std::fs::write(&settings_path, serde_json::to_vec(&configured_settings())?)?;
    let command = retry(&id, &old.assignment_id, "approved");
    let result = api(&controller, "audit/attempt/control", command.clone())
        .await
        .expect("retry");
    let operation = result["replacementOperationId"].as_str().unwrap();
    assert_ne!(operation, old.operation_id);
    assert_ne!(result["replacementAssignmentId"], old.assignment_id);
    assert_eq!(store.attempt_input(operation).await?, retained);
    assert!(store.execution(operation).await?.is_none());
    controller.drive_operation(operation).await?;
    let new = store.run_for_operation(operation).await?;
    let fresh = super::attempts::capture(&controller, &fake, &new).await?;
    assert_ne!(fresh.identity().session_id, capture.identity().session_id);
    assert_eq!(new.snapshot_id, old.snapshot_id);
    assert!(store.activity(&new.assignment_id, 0, 64).await?.is_empty());
    assert_eq!(fake.starts.load(Ordering::SeqCst), 4);
    // Durable action replay remains the same decision even after later drift.
    std::fs::write(&settings_path, b"{\"version\":999}")?;
    assert_eq!(
        api(&controller, "audit/attempt/control", command)
            .await
            .expect("action replay"),
        result
    );
    Ok(())
}

#[tokio::test]
async fn invalid_settings_and_queued_drift_cannot_dispatch() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let (controller, fake, _) = fixture(&temp).await;
    std::fs::create_dir_all(controller.root.as_path())?;
    let path = controller.root.join("reviewer-backends.json");
    std::fs::write(&path, b"{\"version\":1,\"secret\":\"sentinel\"}")?;
    assert!(
        api(
            &controller,
            "audit/start",
            request(&controller, "invalid-settings")
        )
        .await
        .is_err()
    );
    assert!(
        controller
            .store()
            .await?
            .list_run_ids("", 10)
            .await?
            .is_empty()
    );
    std::fs::write(&path, b"{\"version\":1}")?;
    let started = api(
        &controller,
        "audit/start",
        request(&controller, "queued-drift"),
    )
    .await
    .expect("start");
    let mut malformed = configured_settings();
    malformed["claudeSubscription"]["profile"] = json!("relative");
    std::fs::write(&path, serde_json::to_vec(&malformed)?)?;
    controller.tick().await?;
    assert_eq!(fake.starts.load(Ordering::SeqCst), 0);
    let run = controller
        .store()
        .await?
        .load_run_metadata(started["run"]["runId"].as_str().unwrap())
        .await?;
    assert_eq!(run.assignment_state, AssignmentState::Failed);
    assert!(
        controller
            .store()
            .await?
            .execution(&run.operation_id)
            .await?
            .is_none()
    );
    Ok(())
}

#[tokio::test]
async fn queued_native_policy_survives_each_unrelated_route_edit_without_duplicate_work()
-> anyhow::Result<()> {
    for (section, field) in [
        ("claudeSubscription", "clientVersion"),
        ("claudeApi", "clientVersion"),
        ("ollamaLocal", "modelDigest"),
    ] {
        let temp = tempfile::tempdir()?;
        let (controller, fake, _) = fixture(&temp).await;
        std::fs::create_dir_all(controller.root.as_path())?;
        let path = controller.root.join("reviewer-backends.json");
        let mut settings = configured_settings();
        std::fs::write(&path, serde_json::to_vec(&settings)?)?;
        let params = request(&controller, "unrelated-edit");
        let started = api(&controller, "audit/start", params.clone())
            .await
            .expect("start");
        let store = controller.store().await?;
        let run = store
            .load_run_metadata(started["run"]["runId"].as_str().unwrap())
            .await?;
        let approved = store.attempt_input(&run.operation_id).await?;
        settings[section][field] = json!("changed");
        std::fs::write(&path, serde_json::to_vec(&settings)?)?;
        controller.tick().await?;
        assert_eq!(fake.starts.load(Ordering::SeqCst), 1, "{section}");
        let accepted = store.run_for_operation(&run.operation_id).await?;
        assert_eq!(accepted.operation_state, OperationState::Accepted);
        // This fake acknowledges work; its capture/runtime binding is seeded explicitly.
        super::attempts::capture(&controller, &fake, &accepted).await?;
        controller.tick().await?;
        assert_eq!(
            store
                .run_for_operation(&run.operation_id)
                .await?
                .operation_state,
            OperationState::Accepted
        );
        assert_eq!(
            api(&controller, "audit/start", params)
                .await
                .expect("replay")["run"]["runId"],
            run.run_id
        );
        assert_eq!(fake.starts.load(Ordering::SeqCst), 1);
        assert_eq!(store.attempts(&run.run_id).await?.len(), 1);
        assert_eq!(store.attempt_input(&run.operation_id).await?, approved);
    }
    Ok(())
}

async fn set_input(
    store: &AuditStore,
    attempt: &AuditRunRecord,
    input: &Value,
) -> anyhow::Result<()> {
    set_input_raw(store, attempt, &serde_json::to_string(input)?).await
}

async fn set_input_raw(
    store: &AuditStore,
    attempt: &AuditRunRecord,
    raw: &str,
) -> anyhow::Result<()> {
    let digest = sha256_hex(raw.as_bytes());
    sqlx::query(
        "UPDATE audit_attempt_inputs SET request_json=?,input_digest=? WHERE assignment_id=?",
    )
    .bind(raw)
    .bind(&digest)
    .bind(&attempt.assignment_id)
    .execute(&store.pool_for_tests())
    .await?;
    sqlx::query("UPDATE operations SET input_digest=? WHERE id=?")
        .bind(digest)
        .bind(&attempt.operation_id)
        .execute(&store.pool_for_tests())
        .await?;
    Ok(())
}

async fn evidence_fixture(
    route: AuditAuthenticationRoute,
) -> anyhow::Result<(
    tempfile::TempDir,
    Arc<AuditController>,
    AuditRunRecord,
    AdmissionIdentity,
)> {
    let (temp, controller, fake, _id, attempts) = super::attempts::setup().await?;
    let attempt = attempts[0].clone();
    let capture = super::attempts::capture(&controller, &fake, &attempt).await?;
    let store = controller.store().await?;
    let mut input: Value =
        serde_json::from_str(&store.attempt_input(&attempt.operation_id).await?.0)?;
    input["authenticationRoute"] = serde_json::to_value(&route)?;
    input["bindings"]["backendConfigurationIdentity"] =
        json!(BackendSettings::default().identity(&route)?);
    input["executionAdapter"] = json!(if matches!(
        route,
        AuditAuthenticationRoute::ClaudeApi | AuditAuthenticationRoute::ClaudeSubscription
    ) {
        "claude_client"
    } else {
        "native"
    });
    set_input(store, &attempt, &input).await?;
    let owner = AdmissionIdentity {
        run_id: attempt.run_id.clone(),
        assignment_id: attempt.assignment_id.clone(),
        operation_id: attempt.operation_id.clone(),
        execution_id: capture.identity().session_id.clone(),
        owner: controller.owner.clone(),
        owner_generation: attempt.owner_generation,
        run_stop_generation: store.run_stop_generation(&attempt.run_id).await?,
    };
    Ok((temp, controller, attempt, owner))
}

#[tokio::test]
async fn retained_backend_bindings_refuse_only_own_route_changes_and_missing_settings()
-> anyhow::Result<()> {
    for route in [
        AuditAuthenticationRoute::DaemonConfiguredNative,
        AuditAuthenticationRoute::ClaudeSubscription,
        AuditAuthenticationRoute::ClaudeApi,
        AuditAuthenticationRoute::OllamaLocal,
    ] {
        let (_temp, controller, attempt, _) = evidence_fixture(route.clone()).await?;
        let store = controller.store().await?;
        let path = controller.root.join("reviewer-backends.json");
        let approved = configured_settings();
        std::fs::write(&path, serde_json::to_vec(&approved)?)?;
        let settings = BackendSettings::load(controller.root.as_path())?;
        let mut input: Value =
            serde_json::from_str(&store.attempt_input(&attempt.operation_id).await?.0)?;
        input["bindings"]["backendConfigurationIdentity"] = json!(settings.identity(&route)?);
        set_input(store, &attempt, &input).await?;
        let retained = RetainedInput::decode(&store.attempt_input(&attempt.operation_id).await?.0)?;
        retained.validate_backend_configuration(&settings)?;
        if route != AuditAuthenticationRoute::DaemonConfiguredNative {
            assert!(retained.require_native().is_err());
        }
        for (changed_route, section, field) in [
            (
                AuditAuthenticationRoute::ClaudeSubscription,
                "claudeSubscription",
                "clientVersion",
            ),
            (AuditAuthenticationRoute::ClaudeApi, "claudeApi", "upstream"),
            (
                AuditAuthenticationRoute::OllamaLocal,
                "ollamaLocal",
                "modelDigest",
            ),
        ] {
            for remove in [false, true] {
                let mut changed = approved.clone();
                if remove {
                    changed.as_object_mut().unwrap().remove(section);
                } else {
                    changed[section][field] = json!(if field == "upstream" {
                        "https://changed.example.test"
                    } else {
                        "changed"
                    });
                }
                std::fs::write(&path, serde_json::to_vec(&changed)?)?;
                let current = BackendSettings::load(controller.root.as_path())?;
                assert_eq!(
                    retained.validate_backend_configuration(&current).is_ok(),
                    route != changed_route,
                    "retained {route:?}, changed {changed_route:?}, removed {remove}"
                );
            }
        }
        std::fs::write(&path, b"{\"version\":2}")?;
        assert!(BackendSettings::load(controller.root.as_path()).is_err());
    }
    Ok(())
}

async fn complete(controller: &AuditController, attempt: &AuditRunRecord) -> anyhow::Result<()> {
    let store = controller.store().await?;
    let current = store.run_for_operation(&attempt.operation_id).await?;
    let snapshot = controller
        .snapshots()
        .open(&SnapshotId::parse(attempt.snapshot_id.clone())?)
        .await?;
    store
        .commit_seeded_result(
            &attempt.operation_id,
            current.attempt_revision,
            &snapshot,
            &empty_result(attempt),
        )
        .await?;
    Ok(())
}

#[tokio::test]
async fn strict_routes_require_bound_verified_generation_identity() -> anyhow::Result<()> {
    for route in [
        AuditAuthenticationRoute::DaemonConfiguredNative,
        AuditAuthenticationRoute::ClaudeApi,
        AuditAuthenticationRoute::OllamaLocal,
    ] {
        for case in [
            "valid",
            "missing",
            "wrong",
            "unverified",
            "connection",
            "wrong_execution",
            "no_admission",
        ] {
            let (_temp, controller, attempt, owner) = evidence_fixture(route.clone()).await?;
            let store = controller.store().await?;
            if case != "no_admission" {
                let now = chrono::Utc::now().timestamp_millis();
                store
                    .admit_generation(&owner, "generation", "fixture", now)
                    .await?;
                assert!(
                    store
                        .mark_generation_send(&owner, "generation", now)
                        .await?
                );
                let model =
                    RetainedInput::decode(&store.attempt_input(&attempt.operation_id).await?.0)?
                        .expected_model()
                        .to_owned();
                let attributed = if case == "connection" {
                    store
                        .check_connection_model(&owner, "generation", &model)
                        .await
                } else {
                    store
                        .attribute_generation(
                            &owner,
                            "generation",
                            match case {
                                "missing" => None,
                                "wrong" => Some("wrong"),
                                _ => Some(&model),
                            },
                            case != "unverified",
                        )
                        .await
                };
                assert_eq!(attributed.is_ok(), !matches!(case, "missing" | "wrong"));
                if case == "wrong_execution" {
                    sqlx::query("UPDATE audit_request_admissions SET execution_id='unbound' WHERE request_id='generation'").execute(&store.pool_for_tests()).await?;
                }
            }
            assert_eq!(
                complete(&controller, &attempt).await.is_ok(),
                case == "valid",
                "{route:?} {case}"
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn subscription_completion_uses_bound_terminal_and_stable_messages_without_admissions()
-> anyhow::Result<()> {
    for case in [
        "valid",
        "no_messages",
        "missing_model",
        "wrong_model",
        "unverified_model",
        "no_terminal",
        "unsuccessful",
        "unverified_terminal",
        "wrong_result",
        "wrong_execution",
        "message_conflict",
        "terminal_conflict",
        "attribution_failure",
    ] {
        let (_temp, controller, attempt, owner) =
            evidence_fixture(AuditAuthenticationRoute::ClaudeSubscription).await?;
        let store = controller.store().await?;
        let model = RetainedInput::decode(&store.attempt_input(&attempt.operation_id).await?.0)?
            .expected_model()
            .to_owned();
        let raw = empty_result(&attempt);
        if case != "no_terminal" {
            store
                .record_client_terminal(
                    &owner,
                    "terminal",
                    case != "unsuccessful",
                    case != "unverified_terminal",
                    if case == "wrong_result" {
                        b"other bytes"
                    } else {
                        &raw
                    },
                )
                .await?;
        }
        if case == "terminal_conflict" {
            assert!(
                store
                    .record_client_terminal(&owner, "changed", true, true, &empty_result(&attempt))
                    .await
                    .is_err()
            );
        } else if case != "no_messages" {
            let observed = store
                .observe_client_message(
                    &owner,
                    "message",
                    match case {
                        "missing_model" => None,
                        "wrong_model" => Some("wrong"),
                        _ => Some(&model),
                    },
                    case != "unverified_model",
                )
                .await;
            assert_eq!(
                observed.is_ok(),
                !matches!(case, "missing_model" | "wrong_model" | "unverified_model")
            );
            if case == "valid" {
                store
                    .observe_client_message(&owner, "message", Some(&model), true)
                    .await?;
                let evidence = store.model_evidence(&attempt.assignment_id).await?;
                assert_eq!(evidence.len(), 1);
                assert_eq!(evidence[0].evidence_scope.as_deref(), Some("message"));
                assert!(
                    store
                        .admit_generation(
                            &owner,
                            "forbidden",
                            "fixture",
                            chrono::Utc::now().timestamp_millis()
                        )
                        .await
                        .is_err()
                );
            }
            if case == "message_conflict" {
                assert!(
                    store
                        .observe_client_message(&owner, "message", Some("changed"), true)
                        .await
                        .is_err()
                );
            }
        }
        if case == "wrong_execution" {
            sqlx::query("UPDATE audit_client_terminal_evidence SET execution_id='unbound'")
                .execute(&store.pool_for_tests())
                .await?;
        }
        if case == "attribution_failure" {
            sqlx::query(
                "UPDATE audit_attempts SET attribution_failure='seeded' WHERE assignment_id=?",
            )
            .bind(&attempt.assignment_id)
            .execute(&store.pool_for_tests())
            .await?;
        }
        assert_eq!(
            complete(&controller, &attempt).await.is_ok(),
            case == "valid",
            "{case}"
        );
        assert!(
            store
                .generation_records(&attempt.assignment_id)
                .await?
                .is_empty()
        );
        if case == "valid" {
            for peer in store.attempts(&attempt.run_id).await?.into_iter().skip(1) {
                store
                    .control_attempt(
                        &crate::audex::store::attempts::ControlRequest {
                            run_id: peer.run_id,
                            assignment_id: peer.assignment_id,
                            action_key: format!("exclude-{}", peer.operation_id),
                            action: crate::audex::store::attempts::AttemptAction::Ignore,
                            reason: "fixture complete collection".to_owned(),
                            authorise_retry: false,
                            acknowledge_possible_overlap: false,
                        },
                        None,
                    )
                    .await?;
            }
            let frozen = store.freeze_report(&attempt.run_id).await?;
            assert!(frozen.reviewers[0].observation_verified);
            assert_eq!(
                frozen.reviewers[0].observed_model.as_deref(),
                Some(model.as_str())
            );
            assert!(
                store
                    .generation_records(&attempt.assignment_id)
                    .await?
                    .is_empty()
            );
        }
        let reopened = AuditStore::open(controller.root.clone()).await?;
        assert_eq!(
            reopened.model_evidence(&attempt.assignment_id).await?,
            store.model_evidence(&attempt.assignment_id).await?
        );
    }
    Ok(())
}

#[tokio::test]
async fn subscription_evidence_never_completes_a_strict_route_and_wrong_owners_are_refused()
-> anyhow::Result<()> {
    for route in [
        AuditAuthenticationRoute::DaemonConfiguredNative,
        AuditAuthenticationRoute::ClaudeApi,
        AuditAuthenticationRoute::OllamaLocal,
    ] {
        let (_temp, controller, attempt, owner) =
            evidence_fixture(AuditAuthenticationRoute::ClaudeSubscription).await?;
        let store = controller.store().await?;
        let mut input: Value =
            serde_json::from_str(&store.attempt_input(&attempt.operation_id).await?.0)?;
        let model = RetainedInput::decode(&input.to_string())?
            .expected_model()
            .to_owned();
        for field in 0..4 {
            let mut wrong = owner.clone();
            match field {
                0 => wrong.execution_id = "wrong".into(),
                1 => wrong.owner_generation += 1,
                2 => wrong.operation_id = "wrong".into(),
                _ => wrong.run_stop_generation += 1,
            }
            assert!(
                store
                    .observe_client_message(&wrong, "wrong-owner", Some(&model), true)
                    .await
                    .is_err()
            );
            assert!(
                store
                    .record_client_terminal(
                        &wrong,
                        "wrong-owner",
                        true,
                        true,
                        &empty_result(&attempt)
                    )
                    .await
                    .is_err()
            );
        }
        store
            .observe_client_message(&owner, "message", Some(&model), true)
            .await?;
        store
            .record_client_terminal(&owner, "terminal", true, true, &empty_result(&attempt))
            .await?;
        input["authenticationRoute"] = serde_json::to_value(&route)?;
        input["bindings"]["backendConfigurationIdentity"] =
            json!(BackendSettings::default().identity(&route)?);
        input["executionAdapter"] = json!(if route == AuditAuthenticationRoute::ClaudeApi {
            "claude_client"
        } else {
            "native"
        });
        set_input(store, &attempt, &input).await?;
        assert!(complete(&controller, &attempt).await.is_err(), "{route:?}");
        assert!(
            store
                .model_evidence(&attempt.assignment_id)
                .await?
                .is_empty()
        );
        assert!(
            store
                .generation_records(&attempt.assignment_id)
                .await?
                .is_empty()
        );
    }
    Ok(())
}

#[tokio::test]
async fn native_destination_and_model_policy_drift_refuse_retry_without_replacing_attempt()
-> anyhow::Result<()> {
    let (_temp, controller, fake, id, attempts) = super::attempts::setup().await?;
    let store = controller.store().await?;
    let mut config = controller.config.as_ref().clone();
    let provider = config.model_provider_id.clone();
    config.model_provider.base_url = Some("http://127.0.0.1:1/changed".to_owned());
    if let Some(p) = config.model_providers.get_mut(&provider) {
        p.base_url = config.model_provider.base_url.clone();
    }
    let changed = AuditController::new(
        Arc::new(config),
        controller.models.clone(),
        fake.clone(),
        Arc::new(|_| {}),
    );
    assert!(
        api(
            &changed,
            "audit/attempt/control",
            retry(&id, &attempts[0].assignment_id, "changed-endpoint")
        )
        .await
        .is_err()
    );
    let mut model = codex_models_manager::model_info::model_info_from_slug(&attempts[0].model);
    model.display_name = "Changed policy catalogue".to_owned();
    let models = Arc::new(StaticModelsManager::new(
        None,
        ModelsResponse {
            models: vec![model],
        },
        ModelOrigin::ExplicitConfig,
    ));
    let changed = AuditController::new(controller.config.clone(), models, fake, Arc::new(|_| {}));
    assert!(
        api(
            &changed,
            "audit/attempt/control",
            retry(&id, &attempts[0].assignment_id, "changed-model-policy")
        )
        .await
        .is_err()
    );
    assert_eq!(store.attempts(&id).await?.len(), 3);
    assert!(store.attempts(&id).await?.iter().all(|a| a.current));
    Ok(())
}

#[tokio::test]
async fn same_action_cannot_select_an_unavailable_route_even_with_backend_settings()
-> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let (controller, fake, _) = fixture(&temp).await;
    api(
        &controller,
        "audit/start",
        request(&controller, "route-change"),
    )
    .await
    .expect("native");
    std::fs::write(
        controller.root.join("reviewer-backends.json"),
        serde_json::to_vec(&configured_settings())?,
    )?;
    for (route, adapter) in [
        ("claude_subscription", "claude_client"),
        ("claude_api", "claude_client"),
        ("ollama_local", "native"),
    ] {
        for key in ["route-change", "new-unavailable"] {
            let mut changed = request(&controller, key);
            changed.as_object_mut().unwrap().remove("provider");
            changed.as_object_mut().unwrap().remove("model");
            changed["reviewers"] = json!([{"provider":"fixture","model":"audit-test","executionAdapter":adapter,"authenticationRoute":route}]);
            let error = api(&controller, "audit/start", changed)
                .await
                .expect_err("external route");
            if key == "route-change" {
                assert_eq!(error.data.unwrap()["reason"], "auditIdempotencyConflict");
            }
        }
    }
    assert_eq!(
        controller.store().await?.list_run_ids("", 10).await?.len(),
        1
    );
    assert_eq!(fake.starts.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test]
async fn retry_uses_sealed_source_after_original_file_changes_and_refuses_changed_policy_versions()
-> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let (controller, fake, _) = fixture(&temp).await;
    let source = temp.path().join("original.rs");
    std::fs::write(&source, b"fn original() {}\n")?;
    let mut params = request(&controller, "sealed-retry");
    params["source"] = json!({"type":"file","name":"example.rs","path":source});
    let started = api(&controller, "audit/start", params)
        .await
        .expect("start");
    let id = started["run"]["runId"].as_str().unwrap();
    let store = controller.store().await?;
    let original = store.load_run_metadata(id).await?;
    let approved = store.attempt_input(&original.operation_id).await?;
    std::fs::write(&source, b"fn changed() {}\n")?;
    for (ordinal, (field, value)) in [
        ("version", json!(999)),
        ("limits", json!({})),
        ("toolExposure", json!(["shell"])),
    ]
    .into_iter()
    .enumerate()
    {
        let mut input: Value = serde_json::from_str(&approved.0)?;
        input["bindings"][field] = value;
        set_input(store, &original, &input).await?;
        assert!(
            api(
                &controller,
                "audit/attempt/control",
                retry(
                    id,
                    &original.assignment_id,
                    &format!("invalid-policy-{ordinal}")
                )
            )
            .await
            .is_err()
        );
        assert_eq!(store.attempts(id).await?.len(), 1);
    }
    set_input_raw(store, &original, &approved.0).await?;
    let result = api(
        &controller,
        "audit/attempt/control",
        retry(id, &original.assignment_id, "sealed-source"),
    )
    .await
    .expect("approved source retained");
    let replacement = store
        .run_for_operation(result["replacementOperationId"].as_str().unwrap())
        .await?;
    assert_eq!(replacement.snapshot_id, original.snapshot_id);
    assert_eq!(
        store.attempt_input(&replacement.operation_id).await?,
        approved
    );
    let snapshot = controller
        .snapshots()
        .open(&SnapshotId::parse(replacement.snapshot_id)?)
        .await?;
    let bytes = snapshot
        .access(crate::audex::snapshot::AccessLimits::default())
        .read(&SnapshotPath::new("example.rs")?, 0, 64)
        .await?;
    assert_eq!(bytes.bytes, b"fn original() {}\n");
    assert_eq!(fake.starts.load(Ordering::SeqCst), 0);
    Ok(())
}
