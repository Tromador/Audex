use super::*;
use crate::audex::conversation_evidence::ConversationEvidence;

async fn evidence(controller: &AuditController, run: &str, query: Value) -> Value {
    api(
        controller,
        "audit/evidence",
        json!({"runId":run,"query":query}),
    )
    .await
    .unwrap()["data"]
        .clone()
}

#[tokio::test]
async fn file_list_admission_is_atomic_and_preflight_detects_any_changed_file() -> anyhow::Result<()>
{
    let temp = tempfile::tempdir()?;
    let (controller, fake, _) = fixture(&temp).await;
    let a = temp.path().join("a.rs");
    let b = temp.path().join("b.rs");
    std::fs::write(&a, "a\n")?;
    std::fs::write(&b, "b\n")?;
    let request = json!({"requestKey":"files", "source":{"type":"fileList","files":[{"name":"a.rs","path":a},{"name":"b.rs","path":b}]},
        "brief":"audit", "provider":controller.config.model_provider_id,"model":"audit-test"});
    for files in [
        json!([]),
        json!([{"name":"same.rs","path":a},{"name":"same.rs","path":b}]),
        json!([{"name":"a.rs","path":a},{"name":"missing.rs","path":temp.path().join("missing")}]),
        json!([{"name":"../bad.rs","path":a}]),
    ] {
        let mut invalid = request.clone();
        invalid["source"]["files"] = files;
        assert!(api(&controller, "audit/start", invalid).await.is_err());
    }
    assert_eq!(
        api(&controller, "audit/list", json!({})).await.unwrap()["runs"],
        json!([])
    );
    let approved = api(&controller, "audit/preflight", json!({"request":request}))
        .await
        .unwrap();
    std::fs::write(&b, "changed after disclosure\n")?;
    let mut changed = request;
    changed["preflight"] = approved;
    assert!(api(&controller, "audit/start", changed).await.is_err());
    assert_eq!(fake.starts.load(Ordering::SeqCst), 0);
    assert_eq!(
        api(&controller, "audit/list", json!({})).await.unwrap()["runs"],
        json!([])
    );
    Ok(())
}

#[tokio::test]
async fn reviewers_retain_per_file_citations_and_follow_up_uses_the_original_set()
-> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let (controller, fake, _) = fixture(&temp).await;
    let sources = [
        ("one/shared.rs", "one\nneedle client\n", 4u64),
        ("two/shared.rs", "two\ncontext\nneedle server\n", 12),
        ("main.rs", "needle main\n", 0),
    ];
    let mut files = Vec::new();
    for (name, bytes, _) in sources {
        let path = temp.path().join(name);
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(&path, bytes)?;
        files.push(json!({"name":name,"path":path}));
    }
    let started = api(&controller, "audit/start", json!({"requestKey":"retained-files","source":{"type":"fileList","files":files},
        "brief":"cross-file audit", "reviewers":[{"provider":controller.config.model_provider_id,"model":"first"},{"provider":controller.config.model_provider_id,"model":"second"}]})).await.unwrap();
    let id = started["run"]["runId"].as_str().unwrap();
    let snapshot = started["run"]["snapshotId"].as_str().unwrap();
    let store = controller.store().await?;
    for attempt in store.attempts(id).await? {
        assert_eq!(attempt.snapshot_id, snapshot);
    }
    for (name, _, _) in sources {
        std::fs::write(temp.path().join(name), "REPLACED LIVE FILE")?;
    }
    for _ in 0..4 {
        controller.tick().await?;
        for attempt in store.attempts(id).await? {
            if attempt.assignment_state != AssignmentState::Running {
                continue;
            }
            attest_fake_completion(store, &attempt).await?;
            let mut result: Value = serde_json::from_slice(&empty_result(&attempt))?;
            result["summary"] = json!("Cross-file fixture opinion");
            result["findings"] = json!([{"severity":"high","confidence":"high","title":"Cross-file defect","mechanism":"Caller and callee disagree","impact":"Incorrect result","remediation":null,
                "evidence":sources.iter().map(|(name, _, offset)| json!({"description":"Per-file citation","path":name,"start_byte":offset,"end_byte":offset+6})).collect::<Vec<_>>()}]);
            fake.replace(
                &attempt.operation_id,
                WorkState::Completed(serde_json::to_vec(&result)?),
            );
        }
        controller.tick().await?;
    }
    controller.tick().await?;
    let bridge = ConversationEvidence::open(controller.root.as_path(), id).await?;
    let all = evidence(&controller, id, json!({"view":"findings"})).await;
    assert_eq!(
        all,
        bridge.execute("evidence", r#"{"view":"findings"}"#).await?
    );
    assert_eq!(all["total"], 2);
    let mut retained_findings = Vec::new();
    for attempt in store.attempts(id).await? {
        let retained = store.run_for_operation(&attempt.operation_id).await?;
        assert_eq!(retained.findings.len(), 1);
        retained_findings.extend(retained.findings);
    }
    assert_eq!(retained_findings.len(), 2);
    for finding in &retained_findings {
        for (citation, (path, _, _)) in finding.evidence.iter().zip(sources) {
            assert_eq!(citation.manifest_path, path);
            assert!(citation.lines_verified);
            assert_eq!(
                citation.start_line,
                Some(match path {
                    "one/shared.rs" => 2,
                    "two/shared.rs" => 3,
                    _ => 1,
                })
            );
        }
    }
    let starts = fake.starts.load(Ordering::SeqCst);
    for (name, bytes, _) in sources {
        let read = evidence(
            &controller,
            id,
            json!({"view":"source","snapshot":snapshot,"path":name}),
        )
        .await;
        assert_eq!(read["content"], bytes);
        let claude = bridge
            .execute(
                "read",
                &json!({"path":name,"offset":0,"max_bytes":100}).to_string(),
            )
            .await?;
        assert_eq!(claude["content"], bytes);
    }
    assert_eq!(fake.starts.load(Ordering::SeqCst), starts);
    Ok(())
}
