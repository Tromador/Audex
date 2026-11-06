//! Protocol-8 preflight must remain read-only and bind the whole commissioning decision.
use super::*;
mod legacy;
#[path = "../../../../../cli/tests/audit/subscription_fixture.rs"]
mod subscription;

fn request(controller: &AuditController) -> Value {
    json!({"requestKey":"public-preflight","source":{"type":"snippet","name":"example.rs","content":"fn main() {}\n"},
        "brief":"Review the sealed source", "audexGuidance":"Explicit guidance", "agentsEvidence":"Explicit evidence",
        "reviewers":[{"provider":controller.config.model_provider_id,"model":"audit-test"}, subscription::selection()],
        "scheduling":"sequential"})
}

#[tokio::test]
async fn native_preview_has_no_store_snapshot_or_dispatch_side_effects() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let (controller, fake, _) = fixture(&temp).await;
    let mut params = request(&controller);
    params["reviewers"].as_array_mut().unwrap().pop();
    let first = api(&controller, "audit/preflight", json!({"request":params}))
        .await
        .map_err(|error| anyhow::anyhow!("{error:?}"))?;
    let second = api(&controller, "audit/preflight", json!({"request":params}))
        .await
        .map_err(|error| anyhow::anyhow!("{error:?}"))?;
    assert_eq!(first, second);
    assert_eq!(first["protocolVersion"], 8);
    assert_eq!(first["reviewers"][0]["resolvedModel"], "audit-test");
    assert!(first["identity"].as_str().unwrap().len() == 64);
    assert!(!controller.root.exists());
    assert!(controller.store.get().is_none());
    assert_eq!(fake.starts.load(Ordering::SeqCst), 0);
    for route in ["claude_api", "ollama_local"] {
        let mut bad = params.clone();
        let mut selection = subscription::selection();
        selection["authenticationRoute"] = json!(route);
        if route == "ollama_local" {
            selection["executionAdapter"] = json!("native");
        }
        bad["reviewers"].as_array_mut().unwrap().push(selection);
        assert!(
            api(&controller, "audit/preflight", json!({"request":bad}))
                .await
                .is_err()
        );
        assert!(!controller.root.exists());
    }
    Ok(())
}

#[tokio::test]
async fn subscription_preview_binds_local_policy_and_refuses_every_drift_before_commissioning()
-> anyhow::Result<()> {
    for drift in [
        "executable",
        "profile",
        "runtime",
        "version",
        "model",
        "source",
        "brief",
        "context",
        "order",
        "missing_preflight",
    ] {
        let temp = tempfile::tempdir()?;
        let (controller, fake, _) = fixture(&temp).await;
        let child = subscription::configure(temp.path(), "valid")?;
        std::fs::create_dir(controller.root.join("c71-system-policy"))?;
        let mut params = request(&controller);
        let before = std::fs::read(child.join("subscription-profile/settings.json"))?;
        let preview = api(&controller, "audit/preflight", json!({"request":params}))
            .await
            .map_err(|error| anyhow::anyhow!("{error:?}"))?;
        assert_eq!(
            preview["reviewers"][1]["selection"]["authenticationRoute"],
            "claude_subscription"
        );
        assert_eq!(
            preview["reviewers"][1]["client"]["executable"],
            json!(child.join("client"))
        );
        assert_eq!(
            preview["reviewers"][1]["expectedServedModel"],
            "claude-haiku-5-5"
        );
        assert!(!child.join("launches").exists());
        assert!(!child.join("policy-control-count").exists());
        assert_eq!(subscription::prompts(&child), 0);
        assert_eq!(
            std::fs::read(child.join("subscription-profile/settings.json"))?,
            before
        );
        params["preflight"] = preview.clone();
        match drift {
            "executable" => {
                use std::io::Write;
                std::fs::OpenOptions::new()
                    .append(true)
                    .open(child.join("client"))?
                    .write_all(b"\n# drift\n")?;
            }
            "profile" => {
                let mut settings: Value = serde_json::from_slice(&before)?;
                settings["theme"] = json!("light");
                std::fs::write(
                    child.join("subscription-profile/settings.json"),
                    settings.to_string(),
                )?;
            }
            "runtime" | "version" => {
                let path = controller.root.join("reviewer-backends.json");
                let mut settings: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
                let key = if drift == "runtime" {
                    "runtimeRoot"
                } else {
                    "clientVersion"
                };
                settings["claudeSubscription"][key] = json!(if drift == "runtime" {
                    "/missing/c73-runtime"
                } else {
                    "999"
                });
                std::fs::write(path, settings.to_string())?;
            }
            "model" => params["reviewers"][1]["expectedServedModel"] = json!("claude-changed"),
            "source" => params["source"]["content"] = json!("fn changed() {}"),
            "brief" => params["brief"] = json!("Changed brief"),
            "context" => params["audexGuidance"] = json!("Changed context"),
            "order" => params["reviewers"].as_array_mut().unwrap().reverse(),
            "missing_preflight" => {
                params.as_object_mut().unwrap().remove("preflight");
            }
            _ => unreachable!(),
        }
        assert!(
            api(&controller, "audit/start", params).await.is_err(),
            "{drift}"
        );
        assert!(controller.store.get().is_none(), "{drift}");
        assert!(!controller.root.join("snapshots").exists(), "{drift}");
        assert!(
            !controller.root.join("audit-state.sqlite3").exists(),
            "{drift}"
        );
        assert!(!child.join("launches").exists(), "{drift}");
        assert_eq!(fake.starts.load(Ordering::SeqCst), 0);
    }
    Ok(())
}

#[tokio::test]
async fn public_subscription_roster_is_atomic_idempotent_and_retry_keeps_approved_bindings()
-> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let (controller, fake, _) = fixture(&temp).await;
    let child = subscription::configure(temp.path(), "valid")?;
    std::fs::create_dir(controller.root.join("c71-system-policy"))?;
    let mut params = request(&controller);
    params["preflight"] = api(&controller, "audit/preflight", json!({"request":params}))
        .await
        .map_err(|error| anyhow::anyhow!("{error:?}"))?;
    let result = api(&controller, "audit/start", params.clone())
        .await
        .map_err(|error| anyhow::anyhow!("{error:?}"))?;
    let id = result["run"]["runId"].as_str().unwrap();
    let store = controller
        .store()
        .await
        .map_err(|error| anyhow::anyhow!("{error:?}"))?;
    let attempts = store
        .attempts(id)
        .await
        .map_err(|error| anyhow::anyhow!("{error:?}"))?;
    assert_eq!(attempts.len(), 2);
    assert_eq!(attempts[0].snapshot_id, attempts[1].snapshot_id);
    let raw = store
        .attempt_input(&attempts[1].operation_id)
        .await
        .map_err(|error| anyhow::anyhow!("{error:?}"))?;
    let retained = crate::audex::retained::RetainedInput::decode(&raw.0)?;
    assert_eq!(
        retained.route(),
        AuditAuthenticationRoute::ClaudeSubscription
    );
    assert_eq!(retained.provider, "anthropic");
    assert!(raw.1.contains("executableDigest"));
    assert_eq!(fake.starts.load(Ordering::SeqCst), 0);
    std::fs::write(child.join("client"), b"changed executable")?;
    let observer = AuditController::new(
        controller.config.clone(),
        controller.models.clone(),
        fake.clone(),
        Arc::new(|_| {}),
    );
    let replay = api(&observer, "audit/preflight", json!({"request":params}))
        .await
        .map_err(|error| anyhow::anyhow!("{error:?}"))?;
    assert!(
        observer.store.get().is_none(),
        "replay preflight never opens a writable store"
    );
    assert_eq!(replay["existingRunId"], id);
    assert_eq!(replay["identity"], params["preflight"]["identity"]);
    params["preflight"] = replay;
    assert_eq!(
        api(&controller, "audit/start", params.clone())
            .await
            .map_err(|error| anyhow::anyhow!("{error:?}"))?["run"]["runId"],
        id,
        "identical replay retrieves the original approval without requalification or execution"
    );
    assert!(api(&controller, "audit/attempt/control", json!({"runId":id,"assignmentId":attempts[1].assignment_id,
        "action":"retry","actionKey":"drift-retry","reason":"Explicit Retry","authoriseRetry":true,"acknowledgePossibleOverlap":true})).await.is_err());
    assert_eq!(
        store
            .attempts(id)
            .await
            .map_err(|error| anyhow::anyhow!("{error:?}"))?
            .len(),
        2
    );
    params["reviewers"][1]["model"] = json!("claude-changed");
    assert_eq!(
        api(&controller, "audit/start", params)
            .await
            .unwrap_err()
            .data
            .unwrap()["reason"],
        "auditIdempotencyConflict"
    );
    assert!(!child.join("launches").exists());
    Ok(())
}
