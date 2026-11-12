use super::*;
use crate::audex::conversation_evidence::ConversationEvidence;

async fn retained(
    controller: &Arc<AuditController>,
    fake: &Fake,
    key: &str,
    agents: Option<&str>,
    audex: Option<&str>,
) -> anyhow::Result<String> {
    let started = api(
        controller,
        "audit/start",
        json!({
            "provider":controller.config.model_provider_id,"model":"audit-test","requestKey":key,
            "source":{"type":"snippet","name":"example.rs","content":"fn main() {}\n"},
            "brief":"Captured guidance fixture","agentsEvidence":agents,"audexGuidance":audex
        }),
    )
    .await
    .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let id = started["run"]["runId"].as_str().unwrap().to_owned();
    let store = controller.store().await?;
    for _ in 0..4 {
        controller.tick().await?;
        for attempt in store.attempts(&id).await? {
            if attempt.assignment_state == AssignmentState::Running {
                attest_fake_completion(store, &attempt).await?;
                fake.replace(
                    &attempt.operation_id,
                    WorkState::Completed(empty_result(&attempt)),
                );
            }
        }
    }
    assert!(store.load_run(&id).await?.phase.is_terminal());
    Ok(id)
}

#[tokio::test]
async fn guidance_is_original_attributed_lossless_and_audit_bound() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let (controller, fake, _) = fixture(&temp).await;
    let original = format!(
        "\u{feff}Captured convention M2\r\n{}\r\nEND",
        "é漢🙂\0".repeat(3000)
    );
    let live = temp.path().join("AGENTS.md");
    std::fs::write(&live, &original)?;
    let id = retained(
        &controller,
        &fake,
        "guidance",
        Some(&std::fs::read_to_string(&live)?),
        Some("Original AUDEX guidance\r\n"),
    )
    .await?;
    let other = retained(
        &controller,
        &fake,
        "other-guidance",
        Some("Foreign M2 convention"),
        None,
    )
    .await?;
    let empty = retained(&controller, &fake, "no-guidance", None, None).await?;
    std::fs::write(&live, "LIVE_GUIDANCE_REPLACED")?;
    let bridge = ConversationEvidence::open(controller.root.as_path(), &id).await?;
    let other_bridge = ConversationEvidence::open(controller.root.as_path(), &other).await?;
    let snapshot = &bridge.identity.snapshot_id;
    let query = json!({"view":"guidance","snapshot":snapshot});
    let starts = fake.starts.load(Ordering::SeqCst);
    let before = controller.store().await?.load_run_metadata(&id).await?;
    let inventory = read(&controller, &id, query.clone()).await?;
    assert_eq!(
        inventory,
        bridge.execute("evidence", &query.to_string()).await?
    );
    assert_eq!(inventory["total"], 2);
    let document = inventory["documents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["path"] == "AGENTS.md")
        .unwrap();
    assert_eq!(document["role"], "agents_evidence");
    assert_eq!(document["sha256"], sha256_hex(original.as_bytes()));
    assert_eq!(document["size"], original.len());
    let full_query = json!({"view":"guidance","snapshot":snapshot,"path":"AGENTS.md"});
    let full = read(&controller, &id, full_query.clone()).await?;
    assert_eq!(
        full,
        bridge.execute("evidence", &full_query.to_string()).await?
    );
    assert_eq!(full["content"], original);
    assert_eq!(full["document"], *document);
    assert_eq!(full["identity"], serde_json::to_value(&bridge.identity)?);
    let mut offset = 0;
    let mut restored = String::new();
    loop {
        let query = json!({"view":"guidance","snapshot":snapshot,"path":"AGENTS.md","offset":offset,"maxBytes":131});
        let page = bridge.execute("evidence", &query.to_string()).await?;
        assert_eq!(page, read(&controller, &id, query).await?);
        assert_eq!(page["document"], *document);
        assert_eq!(page["identity"], full["identity"]);
        restored.push_str(page["content"].as_str().unwrap());
        if page["eof"] == true {
            break;
        }
        offset = page["nextOffset"].as_u64().unwrap();
    }
    assert_eq!(restored.as_bytes(), original.as_bytes());
    let audex = bridge
        .execute(
            "evidence",
            &json!({"view":"guidance","snapshot":snapshot,"path":"AUDEX.md"}).to_string(),
        )
        .await?;
    assert_eq!(audex["content"], "Original AUDEX guidance\r\n");
    assert_eq!(audex["document"]["role"], "audex_guidance");
    for invalid in [
        json!({"snapshot":other_bridge.identity.snapshot_id}),
        json!({"path":"/etc/passwd"}),
        json!({"path":"../AGENTS.md"}),
        json!({"path":"example.rs"}),
        json!({"path":"missing.md"}),
        json!({"offset":1}),
        json!({"maxBytes":0}),
        json!({"offset":original.len()+1}),
    ] {
        let mut query = full_query.clone();
        query
            .as_object_mut()
            .unwrap()
            .extend(invalid.as_object().unwrap().clone());
        assert!(
            read(&controller, &id, query.clone()).await.is_err(),
            "{query}"
        );
        assert!(
            bridge
                .execute("evidence", &query.to_string())
                .await
                .is_err(),
            "{query}"
        );
    }
    for extra in ["runId", "reviewer", "sha256", "write", "limit"] {
        let mut query = full_query.clone();
        query[extra] = json!("foreign");
        assert!(
            bridge
                .execute("evidence", &query.to_string())
                .await
                .is_err()
        );
    }
    assert!(
        other_bridge
            .execute("evidence", &full_query.to_string())
            .await
            .is_err()
    );
    let empty_bridge = ConversationEvidence::open(controller.root.as_path(), &empty).await?;
    let q = json!({"view":"guidance","snapshot":empty_bridge.identity.snapshot_id});
    assert_eq!(
        empty_bridge.execute("evidence", &q.to_string()).await?["documents"],
        json!([])
    );
    let mut q = q;
    q["path"] = json!("AGENTS.md");
    assert!(
        empty_bridge
            .execute("evidence", &q.to_string())
            .await
            .is_err()
    );
    assert_eq!(fake.starts.load(Ordering::SeqCst), starts);
    assert_eq!(
        controller
            .store()
            .await?
            .load_run_metadata(&id)
            .await?
            .revision,
        before.revision
    );
    assert_eq!(std::fs::read_to_string(&live)?, "LIVE_GUIDANCE_REPLACED");
    Ok(())
}

#[tokio::test]
#[cfg(unix)]
async fn guidance_rejects_corruption_missing_objects_and_symlinks() -> anyhow::Result<()> {
    use std::os::unix::fs::{PermissionsExt, symlink};
    for mode in ["content", "missing", "symlink"] {
        let temp = tempfile::tempdir()?;
        let (controller, fake, _) = fixture(&temp).await;
        let text = "Original M2 convention";
        let id = retained(&controller, &fake, mode, Some(text), None).await?;
        let bridge = ConversationEvidence::open(controller.root.as_path(), &id).await?;
        let snapshot = &bridge.identity.snapshot_id;
        let object = controller
            .root
            .join("snapshots")
            .join(snapshot.rsplit(':').next().unwrap())
            .join("objects/sha256")
            .join(sha256_hex(text.as_bytes()));
        let query = json!({"view":"guidance","snapshot":snapshot,"path":"AGENTS.md"});
        if mode == "content" {
            std::fs::set_permissions(&object, std::fs::Permissions::from_mode(0o644))?;
            std::fs::write(&object, "x".repeat(text.len()))?;
            std::fs::set_permissions(&object, std::fs::Permissions::from_mode(0o444))?;
        } else {
            let parent = object.as_path().parent().unwrap();
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o755))?;
            std::fs::remove_file(&object)?;
            if mode == "symlink" {
                let outside = temp.path().join("live-guidance");
                std::fs::write(&outside, text)?;
                symlink(outside, &object)?;
            }
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o555))?;
        }
        assert!(
            bridge
                .execute("evidence", &query.to_string())
                .await
                .is_err(),
            "{mode}"
        );
        assert!(read(&controller, &id, query).await.is_err(), "{mode}");
    }
    Ok(())
}
