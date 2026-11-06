//! Legacy request identity remains authoritative through public preflight and Start.
use super::*;
#[path = "../../../../../../cli/tests/audit/legacy_fixture.rs"]
mod legacy;

#[tokio::test]
async fn legacy_singular_preflight_and_start_preserve_identity() -> anyhow::Result<()> {
    replay(false).await
}

#[tokio::test]
async fn legacy_ordered_roster_preflight_and_start_preserve_identity() -> anyhow::Result<()> {
    replay(true).await
}

async fn replay(roster: bool) -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let (controller, fake, _) = fixture(&temp).await;
    let mut params = json!({"requestKey":"legacy-key", "source":{"type":"snippet","name":"example.rs","content":"fn main() {}\n"},
        "brief":"Original brief", "audexGuidance":"Original guidance", "agentsEvidence":"Original evidence",
        "provider":controller.config.model_provider_id,"model":"audit-test"});
    if roster {
        params.as_object_mut().unwrap().remove("provider");
        params.as_object_mut().unwrap().remove("model");
        params["reviewers"] = json!([
            {"provider":controller.config.model_provider_id,"model":"audit-test"},
            {"provider":controller.config.model_provider_id,"model":"other-test"}]);
    }
    let started = api(&controller, "audit/start", params.clone())
        .await
        .unwrap();
    let id = started["run"]["runId"].as_str().unwrap();
    legacy::retain_v1(temp.path(), id)?;
    let before = legacy::fingerprint(temp.path())?;
    let mut config = (*controller.config).clone();
    config.model_providers.clear();
    config.model_provider_id = "removed-original-provider".into();
    let recovered = AuditController::new(
        Arc::new(config),
        controller.models.clone(),
        fake.clone(),
        Arc::new(|_| {}),
    );
    let preview = api(&recovered, "audit/preflight", json!({"request":params}))
        .await
        .unwrap();
    assert_eq!(preview["existingRunId"], id);
    assert_eq!(
        preview["reviewers"],
        json!([]),
        "do not invent historical configuration bindings"
    );
    assert!(
        recovered.store.get().is_none(),
        "preflight must stay read-only"
    );
    assert_eq!(legacy::fingerprint(temp.path())?, before);
    for _ in 0..2 {
        let mut confirmed = params.clone();
        confirmed["preflight"] = preview.clone();
        assert_eq!(
            api(&recovered, "audit/start", confirmed).await.unwrap()["run"]["runId"],
            id
        );
    }
    // The singular and explicit one-member native forms remain equivalent.
    if !roster {
        let mut typed = params.clone();
        typed.as_object_mut().unwrap().remove("provider");
        typed.as_object_mut().unwrap().remove("model");
        typed["reviewers"] =
            json!([{"provider":controller.config.model_provider_id,"model":"audit-test"}]);
        typed["preflight"] = api(&recovered, "audit/preflight", json!({"request":typed}))
            .await
            .unwrap();
        assert_eq!(
            api(&recovered, "audit/start", typed).await.unwrap()["run"]["runId"],
            id
        );
    }
    for field in [
        "brief",
        "audexGuidance",
        "agentsEvidence",
        "source",
        "name",
        "model",
        "provider",
        "reasoningEffort",
        "expectedServedModel",
        "scheduling",
        "order",
    ] {
        let mut changed = params.clone();
        match field {
            "order" if !roster => continue,
            "order" => changed["reviewers"].as_array_mut().unwrap().reverse(),
            "source" => changed["source"]["content"] = json!("changed"),
            "name" => changed["source"]["name"] = json!("changed.rs"),
            "scheduling" => changed[field] = json!("sequential"),
            "model" | "provider" | "reasoningEffort" | "expectedServedModel" => {
                let selection = if roster {
                    &mut changed["reviewers"][0]
                } else {
                    &mut changed
                };
                selection[field] = json!(if field == "reasoningEffort" {
                    "high"
                } else {
                    "changed"
                });
            }
            _ => changed[field] = json!("changed"),
        }
        for (method, request) in [
            ("audit/preflight", json!({"request":changed})),
            ("audit/start", changed),
        ] {
            let error = api(&recovered, method, request).await.expect_err(field);
            assert_eq!(
                error.data.unwrap()["reason"],
                "auditIdempotencyConflict",
                "{method}: {field}"
            );
        }
    }
    assert_eq!(fake.starts.load(Ordering::SeqCst), 0);
    assert_eq!(
        legacy::fingerprint(temp.path())?,
        before,
        "replay and conflicts must not mutate audit state"
    );
    // Pre-identity records still cannot prove a requested/default distinction.
    sqlx::query("UPDATE audit_inputs SET request_identity_digest=NULL")
        .execute(&controller.store().await?.pool_for_tests())
        .await?;
    assert!(
        api(&recovered, "audit/preflight", json!({"request":params}))
            .await
            .is_err()
    );
    assert!(api(&recovered, "audit/start", params).await.is_err());
    assert_eq!(fake.starts.load(Ordering::SeqCst), 0);
    Ok(())
}
