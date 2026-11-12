use super::*;

#[path = "guidance.rs"]
mod guidance;

#[tokio::test]
async fn retained_diagnostics_preserve_summaries_opinions_paging_and_integrity()
-> anyhow::Result<()> {
    use crate::audex::conversation_evidence::ConversationEvidence;
    const LUNA: &str = "The captured AGENTS.md middle convention M2 matters: it says revision identifiers may be reused and every replace must invalidate the cached first item, so equal revisions do not establish equal contents. The implementation conditionally invalidates only when revisions differ, allowing a stale first item after a same-revision replacement.";
    let temp = tempfile::tempdir()?;
    let (controller, fake, _) = fixture(&temp).await;
    let id = foundations::roster(&controller, "retained-diagnostics", 2).await?;
    assert!(
        read(&controller, &id, json!({"view":"diagnostics"}))
            .await
            .is_err()
    );
    let store = controller.store().await?;
    let opinion = format!(
        "Independent unstructured opinion: {} END_OPINION",
        "é漢🙂".repeat(60_000)
    );
    for _ in 0..4 {
        controller.tick().await?;
        for attempt in store.attempts(&id).await? {
            if attempt.assignment_state != AssignmentState::Running {
                continue;
            }
            attest_fake_completion(store, &attempt).await?;
            let bytes = if attempt.reviewer_ordinal == 0 {
                let mut result: Value = serde_json::from_slice(&empty_result(&attempt))?;
                result["summary"] = json!(LUNA);
                serde_json::to_vec(&result)?
            } else {
                opinion.as_bytes().to_vec()
            };
            fake.replace(&attempt.operation_id, WorkState::Completed(bytes));
        }
        controller.tick().await?;
    }
    controller.tick().await?;
    let retained = store.load_run(&id).await?;
    let report = retained.report.as_ref().unwrap();
    let original = store.read_report(report)?;
    let text = std::str::from_utf8(&original)?;
    assert!(text.contains(LUNA) && text.contains(&opinion));
    let bridge = ConversationEvidence::open(controller.root.as_path(), &id).await?;
    let starts = fake.starts.load(Ordering::SeqCst);
    let full = read(&controller, &id, json!({"view":"diagnostics"})).await?;
    assert_eq!(full["content"], text);
    assert_eq!(full["runId"], full["identity"]["runId"]);
    assert_eq!(full["snapshotId"], full["identity"]["snapshotId"]);
    // The existing MCP encoded-output bound is explicit; callers can page, never truncate.
    assert!(
        bridge
            .execute("evidence", r#"{"view":"diagnostics"}"#)
            .await
            .unwrap_err()
            .to_string()
            .contains("500000")
    );
    let mut offset = 0;
    let mut restored = String::new();
    loop {
        let query = json!({"view":"diagnostics","offset":offset,"maxBytes":65537});
        let claude = bridge.execute("evidence", &query.to_string()).await?;
        assert_eq!(claude, read(&controller, &id, query).await?);
        restored.push_str(claude["content"].as_str().unwrap());
        if claude["eof"] == true {
            break;
        }
        offset = claude["nextOffset"].as_u64().unwrap();
    }
    assert_eq!(restored.as_bytes(), original);
    let roster = read(&controller, &id, json!({"view":"reviewers"})).await?;
    for reviewer in roster["items"].as_array().unwrap() {
        assert!(restored.contains(reviewer["reviewerId"].as_str().unwrap()));
        assert!(restored.contains(reviewer["assignmentId"].as_str().unwrap()));
    }
    assert_eq!(roster["items"][1]["structuredFindingsAvailable"], false);
    assert_eq!(roster["items"][1]["opinionAvailable"], true);
    for query in [
        json!({"view":"diagnostics","runId":"other"}),
        json!({"view":"diagnostics","snapshot":"other"}),
        json!({"view":"diagnostics","reviewer":"other"}),
        json!({"view":"diagnostics","path":"/etc/passwd"}),
    ] {
        assert!(
            bridge
                .execute("evidence", &query.to_string())
                .await
                .is_err()
        );
    }
    assert_eq!(store.read_report(report)?, original);
    assert_eq!(
        store.load_run_metadata(&id).await?.revision,
        retained.revision
    );
    assert_eq!(fake.starts.load(Ordering::SeqCst), starts);
    let path = controller.root.join(&report.relative_path);
    let mut permissions = std::fs::metadata(&path)?.permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    permissions.set_readonly(false);
    std::fs::set_permissions(&path, permissions)?;
    std::fs::write(path, "tampered report")?;
    assert!(
        read(&controller, &id, json!({"view":"diagnostics"}))
            .await
            .is_err()
    );
    assert!(
        bridge
            .execute("evidence", r#"{"view":"diagnostics","maxBytes":128}"#)
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn claude_retained_bridge_reuses_findings_and_rejects_foreign_source_and_writes()
-> anyhow::Result<()> {
    use crate::audex::conversation_evidence::ConversationEvidence;
    let temp = tempfile::tempdir()?;
    let (controller, fake, _) = fixture(&temp).await;
    assert!(
        ConversationEvidence::open(controller.root.as_path(), "missing")
            .await
            .is_err()
    );
    let id = foundations::roster(&controller, "claude-retained", 2).await?;
    assert!(
        ConversationEvidence::open(controller.root.as_path(), &id)
            .await
            .is_err()
    );
    let store = controller.store().await?;
    for _ in 0..4 {
        controller.tick().await?;
        for attempt in store.attempts(&id).await? {
            if attempt.assignment_state != AssignmentState::Running {
                continue;
            }
            attest_fake_completion(store, &attempt).await?;
            let mut result: Value = serde_json::from_slice(&empty_result(&attempt))?;
            result["findings"] = json!((0..2).map(|n| json!({"severity":"high","confidence":"medium",
            "title":format!("Independent finding {n}"),"mechanism":"Captured mechanism","impact":"Claim",
            "remediation":null,"evidence":[{"description":"Source citation","path":"example.rs","start_byte":0,"end_byte":2}]})).collect::<Vec<_>>());
            fake.replace(
                &attempt.operation_id,
                WorkState::Completed(serde_json::to_vec(&result)?),
            );
        }
        controller.tick().await?;
    }
    controller.tick().await?;
    controller.tick().await?;
    let bridge = ConversationEvidence::open(controller.root.as_path(), &id).await?;
    let before = store.load_run_metadata(&id).await?;
    let starts = fake.starts.load(Ordering::SeqCst);
    let all = bridge.execute("evidence", r#"{"view":"findings"}"#).await?;
    assert_eq!(
        all,
        read(&controller, &id, json!({"view":"findings"})).await?
    );
    assert_eq!(all["total"], 4);
    for reviewer in all["reviewers"].as_array().unwrap() {
        let query = json!({"view":"finding","reviewer":reviewer["reviewerId"],"number":2});
        let finding = bridge.execute("evidence", &query.to_string()).await?;
        assert_eq!(finding["reviewer"], *reviewer);
        assert_eq!(finding, read(&controller, &id, query).await?);
    }
    let original = bridge
        .execute(
            "read",
            r#"{"path":"example.rs","offset":0,"max_bytes":100}"#,
        )
        .await?;
    assert_eq!(original["identity"], all["identity"]);
    for (tool, query) in [
        ("evidence", json!({"view":"findings","runId":"another-run"})),
        (
            "evidence",
            json!({"view":"finding","reviewer":"another-reviewer","number":1}),
        ),
        (
            "evidence",
            json!({"view":"source","snapshot":"another-snapshot","path":"example.rs"}),
        ),
        (
            "read",
            json!({"path":"../live.rs","offset":0,"max_bytes":100}),
        ),
        (
            "read",
            json!({"path":"/etc/passwd","offset":0,"max_bytes":100}),
        ),
        (
            "search",
            json!({"query":"x","path":"outside.rs","max_results":10}),
        ),
        ("write", json!({"path":"example.rs","content":"changed"})),
        ("audit_start", json!({})),
    ] {
        assert!(
            bridge.execute(tool, &query.to_string()).await.is_err(),
            "{tool}: {query}"
        );
    }
    let readonly = AuditStore::open_read_only(controller.root.clone()).await?;
    assert!(
        sqlx::query("UPDATE audit_runs SET revision=revision+1 WHERE id=?")
            .bind(&id)
            .execute(&readonly.pool_for_tests())
            .await
            .is_err()
    );
    assert_eq!(fake.starts.load(Ordering::SeqCst), starts);
    assert_eq!(
        store.load_run_metadata(&id).await?.revision,
        before.revision
    );
    let handle = controller
        .snapshots()
        .open(&SnapshotId::parse(bridge.identity.snapshot_id.clone())?)
        .await?;
    let source = handle.source_root().join("example.rs");
    let mut permissions = std::fs::metadata(&source)?.permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    permissions.set_readonly(false);
    std::fs::set_permissions(&source, permissions)?;
    std::fs::write(&source, "tampered sealed source")?;
    assert!(
        bridge
            .execute("read", r#"{"path":"example.rs","offset":0,"max_bytes":1}"#)
            .await
            .is_err()
    );
    assert!(
        bridge
            .execute(
                "search",
                r#"{"query":"tampered","path":null,"max_results":10}"#
            )
            .await
            .is_err()
    );
    Ok(())
}

async fn read(controller: &AuditController, run: &str, query: Value) -> anyhow::Result<Value> {
    let params = json!({"runId":run,"query":query});
    api(controller, "audit/evidence", params)
        .await
        .map(|v| v["data"].clone())
        .map_err(|e| anyhow::anyhow!("{e:?}"))
}

#[tokio::test]
async fn conversation_pages_retain_attribution_and_complete_large_findings() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let (controller, fake, _) = fixture(&temp).await;
    let id = foundations::roster(&controller, "conversation", 6).await?;
    let store = controller.store().await?;
    // Complete each batch through the existing fake dispatch and validated store boundary.
    for _ in 0..6 {
        controller.tick().await?;
        for attempt in store.attempts(&id).await? {
            if attempt.assignment_state != AssignmentState::Running {
                continue;
            }
            attest_fake_completion(store, &attempt).await?;
            let mut result: Value = serde_json::from_slice(&empty_result(&attempt))?;
            result["findings"] = json!((0..7).map(|n| json!({"severity":"high","confidence":"medium",
                "title":format!("Reviewer {} finding {n}",attempt.reviewer_ordinal),"mechanism":"β\\\"\n".repeat(1000),
                "impact":"Independent claim","remediation":null,"evidence":[{"description":"Captured evidence", "path":"example.rs",
                "start_byte":0,"end_byte":2,"start_line":900,"end_line":900}]})).collect::<Vec<_>>());
            fake.replace(
                &attempt.operation_id,
                WorkState::Completed(serde_json::to_vec(&result)?),
            );
        }
        controller.tick().await?;
    }
    controller.tick().await?;
    let before = store.load_run_metadata(&id).await?;
    let starts = fake.starts.load(Ordering::SeqCst);
    let all = read(&controller, &id, json!({"view":"reviewers"})).await?;
    assert_eq!(all["items"].as_array().unwrap().len(), 6);
    assert!(all["nextOffset"].is_null());
    let first = read(&controller, &id, json!({"view":"reviewers","limit":4})).await?;
    assert_eq!(first["total"], 6);
    assert_eq!(first["nextOffset"], 4);
    let last = read(&controller, &id, json!({"view":"reviewers","offset":4})).await?;
    assert_eq!(last["items"].as_array().unwrap().len(), 2);
    assert!(last["nextOffset"].is_null());
    let reviewer = &first["items"][1]["reviewerId"];
    assert_eq!(first["items"][1]["findingCount"], 7);
    let page = read(
        &controller,
        &id,
        json!({"view":"findings","reviewer":reviewer,"limit":4}),
    )
    .await?;
    assert_eq!(page["items"][1]["number"], 2);
    assert_eq!(page["total"], 7);
    let tail = read(
        &controller,
        &id,
        json!({"view":"findings","reviewer":reviewer,"offset":4}),
    )
    .await?;
    assert_eq!(tail["items"].as_array().unwrap().len(), 3);
    assert!(tail["nextOffset"].is_null());
    let mut offset = 0;
    let mut content = String::new();
    loop {
        let part = read(
            &controller,
            &id,
            json!({"view":"finding","reviewer":reviewer,"number":2,"offset":offset,"maxBytes":1300}),
        )
        .await?;
        assert!(part["content"].as_str().unwrap().len() <= 1300);
        assert_eq!(part["reviewer"]["reviewerId"], *reviewer);
        content.push_str(part["content"].as_str().unwrap());
        match part["nextOffset"].as_u64() {
            Some(next) => {
                assert!(next > offset);
                offset = next;
            }
            None => break,
        }
    }
    let finding: Value = serde_json::from_str(&content)?;
    let complete = read(
        &controller,
        &id,
        json!({"view":"finding","reviewer":reviewer,"number":2}),
    )
    .await?;
    assert_eq!(complete["finding"], finding);
    assert_eq!(complete["content"], content);
    assert!(complete["nextOffset"].is_null());
    let collection = read(
        &controller,
        &id,
        json!({"view":"findings","reviewer":reviewer}),
    )
    .await?;
    assert_eq!(collection["items"].as_array().unwrap().len(), 7);
    let mut collection_finding = collection["items"][1].clone();
    assert_eq!(collection_finding["reviewerId"], *reviewer);
    assert_eq!(
        collection_finding["assignmentId"],
        first["items"][1]["assignmentId"]
    );
    for field in ["number", "reviewerId", "assignmentId"] {
        collection_finding.as_object_mut().unwrap().remove(field);
    }
    assert_eq!(collection_finding, finding);
    // One run-level request preserves every original field across all reviewers.
    let full = read(&controller, &id, json!({"view":"findings"})).await?;
    assert_eq!(full["reviewers"], all["items"]);
    assert_eq!(full["identity"], all["identity"]);
    assert_eq!(full["total"], 42);
    assert!(full["nextOffset"].is_null());
    let selection = store.conversation_selection(&id).await?.unwrap();
    let mut expected = Vec::new();
    for reviewer in selection.reviewers {
        for original in reviewer.completed.unwrap().findings {
            let mut item = serde_json::to_value(&original)?;
            item["number"] = json!(original.ordinal + 1);
            item["reviewerId"] = json!(reviewer.reviewer_id);
            item["assignmentId"] = json!(reviewer.assignment_id);
            expected.push(item);
        }
    }
    assert_eq!(full["items"], json!(expected));
    let crossing = read(
        &controller,
        &id,
        json!({"view":"findings","offset":6,"limit":3}),
    )
    .await?;
    assert_eq!(crossing["items"], json!(expected[6..9]));
    assert_eq!(crossing["nextOffset"], 9);
    let end = read(&controller, &id, json!({"view":"findings","offset":42})).await?;
    assert_eq!(end["items"], json!([]));
    assert_eq!(end["reviewers"], all["items"]);
    assert_eq!(finding["mechanism"], "β\\\"\n".repeat(1000));
    assert_eq!(finding["title"], "Reviewer 1 finding 1");
    assert_eq!(finding["evidence"][0]["start_line"], 1);
    assert_eq!(finding["evidence"][0]["lines_verified"], true);
    for query in [
        json!({"view":"finding","reviewer":reviewer,"number":0}),
        json!({"view":"findings","reviewer":"foreign"}),
        json!({"view":"reviewers","offset":7}),
        json!({"view":"findings","offset":43}),
        json!({"view":"findings","limit":0}),
    ] {
        assert!(read(&controller, &id, query).await.is_err());
    }
    assert_eq!(fake.starts.load(Ordering::SeqCst), starts);
    assert_eq!(
        store.load_run_metadata(&id).await?.revision,
        before.revision
    );
    Ok(())
}

#[tokio::test]
async fn conversation_collections_keep_empty_failed_reviewers_and_legacy_access()
-> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let (controller, fake, _) = fixture(&temp).await;
    let id = foundations::roster(&controller, "empty-collections", 2).await?;
    controller.tick().await?;
    let store = controller.store().await?;
    let attempts = store.attempts(&id).await?;
    attest_fake_completion(store, &attempts[0]).await?;
    fake.replace(
        &attempts[0].operation_id,
        WorkState::Completed(empty_result(&attempts[0])),
    );
    // The shared fixture deliberately has one worker; finish the first before the second starts.
    controller.tick().await?;
    controller.tick().await?;
    assert_eq!(
        store.attempts(&id).await?[1].assignment_state,
        AssignmentState::Running
    );
    fake.replace(
        &attempts[1].operation_id,
        WorkState::Failed("fixture failure".into()),
    );
    controller.tick().await?;
    api(&controller, "audit/cancel", json!({"runId":id}))
        .await
        .unwrap();
    controller.tick().await?;
    let collection = read(&controller, &id, json!({"view":"findings"})).await?;
    let roster = read(&controller, &id, json!({"view":"reviewers"})).await?;
    assert_eq!(collection["reviewers"], roster["items"]);
    assert_eq!(collection["reviewers"].as_array().unwrap().len(), 2);
    assert_eq!(
        collection["reviewers"][0]["structuredFindingsAvailable"],
        true
    );
    assert_eq!(
        collection["reviewers"][1]["structuredFindingsAvailable"],
        false
    );
    assert_eq!(collection["items"], json!([]));
    // The historical fallback has no frozen selection. It must still expose full records.
    let legacy = foundations::roster(&controller, "legacy-collection", 1).await?;
    controller.tick().await?;
    let attempt = &store.attempts(&legacy).await?[0];
    attest_fake_completion(store, attempt).await?;
    let mut result: Value = serde_json::from_slice(&empty_result(attempt))?;
    result["findings"] = json!([{"severity":"low","confidence":"medium","title":"Legacy claim",
        "mechanism":"Legacy mechanism","impact":"Legacy impact","remediation":null,
        "evidence":[{"description":"Legacy evidence","path":"example.rs","start_byte":0,"end_byte":2}]}]);
    fake.replace(
        &attempt.operation_id,
        WorkState::Completed(serde_json::to_vec(&result)?),
    );
    controller.tick().await?;
    controller.tick().await?;
    let modern = read(&controller, &legacy, json!({"view":"findings"})).await?;
    sqlx::query("UPDATE audit_runs SET authoritative_selection_id=NULL WHERE id=?")
        .bind(&legacy)
        .execute(&store.pool_for_tests())
        .await?;
    let historical = read(&controller, &legacy, json!({"view":"findings"})).await?;
    assert_eq!(historical["identity"], modern["identity"]);
    assert_eq!(historical["items"], modern["items"]);
    assert_eq!(historical["items"][0]["mechanism"], "Legacy mechanism");
    let individual = read(
        &controller,
        &legacy,
        json!({"view":"finding","reviewer":historical["reviewers"][0]["reviewerId"],"number":1}),
    )
    .await?;
    assert_eq!(individual["finding"]["id"], historical["items"][0]["id"]);
    Ok(())
}

#[tokio::test]
async fn conversation_source_is_sealed_scoped_verified_and_read_only() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let (controller, fake, _) = fixture(&temp).await;
    let live = temp.path().join("live.rs");
    std::fs::write(&live, b"first\r\nsecond\nlast")?;
    let started = api(&controller,"audit/start",json!({"provider":controller.config.model_provider_id,"model":"audit-test","requestKey":"sealed-followup","source":{"type":"file","name":"evidence.rs","path":live},"brief":"fixture"})).await.unwrap();
    let id = started["run"]["runId"].as_str().unwrap();
    let snapshot = started["run"]["snapshotId"].as_str().unwrap();
    assert_eq!(
        read(&controller, id, json!({"view":"reviewers"})).await?["findingsAvailable"],
        false
    );
    let query =
        json!({"view":"source","snapshot":snapshot,"path":"evidence.rs","offset":7,"maxBytes":7});
    let original = read(&controller, id, query.clone()).await?;
    std::fs::write(&live, b"changed working tree")?;
    assert_eq!(original, read(&controller, id, query.clone()).await?);
    assert_eq!(original["content"], "second\n");
    assert_eq!(original["startLine"], 2);
    assert_eq!(original["endLine"], 2);
    assert_eq!(original["identity"]["snapshotId"], snapshot);
    assert_eq!(original["endOffset"], 14);
    // Explicit continuation reaches EOF without re-capture.
    let mut continuation = query.clone();
    continuation["offset"] = json!(14);
    let tail = read(&controller, id, continuation).await?;
    assert_eq!(tail["content"], "last");
    assert_eq!(tail["eof"], true);
    for (field, value) in [
        ("snapshot", json!("foreign")),
        ("path", json!("../live.rs")),
        ("path", json!(live)),
        ("maxBytes", json!(0)),
        ("offset", json!(999)),
    ] {
        let mut invalid = query.clone();
        invalid[field] = value;
        assert!(read(&controller, id, invalid).await.is_err());
    }
    assert_eq!(fake.starts.load(Ordering::SeqCst), 0);
    let handle = controller
        .snapshots()
        .open(&SnapshotId::parse(snapshot.to_owned())?)
        .await?;
    let path = handle.source_root().join("evidence.rs");
    let mut permissions = std::fs::metadata(&path)?.permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    permissions.set_readonly(false);
    std::fs::set_permissions(&path, permissions)?;
    std::fs::write(&path, b"first\r\nTAMPER\nlast")?;
    assert!(read(&controller, id, query).await.is_err());
    Ok(())
}

#[tokio::test]
async fn conversation_source_has_no_audex_read_granularity() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let (controller, fake, _) = fixture(&temp).await;
    let live = temp.path().join("large.rs");
    let original = format!(
        "{}\nfn retained_middle() {{}}\n{}\n// retained end\n",
        "// before\n".repeat(10_000),
        "// after\n".repeat(10_000)
    );
    std::fs::write(&live, &original)?;
    let started = api(&controller, "audit/start", json!({"provider":controller.config.model_provider_id,"model":"audit-test","requestKey":"unrestricted-source","source":{"type":"file","name":"large.rs","path":live},"brief":"fixture"})).await.unwrap();
    let id = started["run"]["runId"].as_str().unwrap();
    let snapshot = started["run"]["snapshotId"].as_str().unwrap();
    let before = controller.store().await?.load_run_metadata(id).await?;
    std::fs::write(&live, "REPLACED_LIVE_SOURCE")?;
    let query = json!({"view":"source","snapshot":snapshot,"path":"large.rs"});
    let complete = read(&controller, id, query.clone()).await?;
    assert_eq!(complete["content"], original);
    assert_eq!(complete["totalBytes"], original.len());
    assert_eq!(complete["eof"], true);
    assert_eq!(complete["sha256"], sha256_hex(original.as_bytes()));
    for size in [801, 65_537, original.len(), usize::MAX] {
        let mut range = query.clone();
        range["offset"] = json!(17);
        range["maxBytes"] = json!(size);
        let actual = read(&controller, id, range).await?;
        let end = 17usize.saturating_add(size).min(original.len());
        assert_eq!(actual["content"], &original[17..end]);
        assert_eq!(actual["endOffset"], end);
    }
    assert_eq!(fake.starts.load(Ordering::SeqCst), 0);
    assert_eq!(
        controller
            .store()
            .await?
            .load_run_metadata(id)
            .await?
            .revision,
        before.revision
    );
    assert_eq!(std::fs::read_to_string(&live)?, "REPLACED_LIVE_SOURCE");
    Ok(())
}
