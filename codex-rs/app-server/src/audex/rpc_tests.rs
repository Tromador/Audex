use super::*;
use serde_json::{Value, json};
use service::{Attempt, Completion, Outcome};
use std::sync::atomic::AtomicUsize;
use tokio::sync::Semaphore;

struct Fake {
    calls: AtomicUsize,
    entered: Semaphore,
}
impl Reviewer for Fake {
    fn validate(&self, _: &ReviewerSelection) -> Result<()> {
        Ok(())
    }
    fn run<'a>(&'a self, attempt: &'a mut Attempt) -> BoxFuture<'a, Result<Outcome>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            attempt.progress(4)?;
            attempt.write_stderr(b"diagnostic sentinel\n")?;
            attempt.write_transcript(b"{\"retained\":true}\n")?;
            self.entered.add_permits(1);
            let model = attempt.status().reviewer.model.clone();
            if model == "zero" {
                return Ok(Outcome {
                    completion: Completion::Failed("invalid model".into()),
                    output: None,
                    structured_output: None,
                    usage: None,
                });
            }
            if model == "wait" {
                attempt.write_output("partial é漢🙂".as_bytes())?;
                attempt.cancellation.cancelled().await;
                return Ok(Outcome {
                    completion: Completion::Cancelled,
                    output: None,
                    structured_output: None,
                    usage: None,
                });
            }
            let finding = json!({"path":"example.rs","start_line":1,"end_line":1,"title":"Fixture finding","severity":"low","confidence":"medium","trigger":"input","mechanism":"mechanism","impact":"impact","evidence":"evidence","remediation":"fix"});
            Ok(Outcome {
                completion: Completion::Succeeded,
                output: Some("literal é漢🙂\n".as_bytes().to_vec()),
                structured_output: Some(
                    json!({"summary":"fake review","findings":[finding.clone(),finding]}),
                ),
                usage: None,
            })
        })
    }
}
struct Fixture {
    home: tempfile::TempDir,
    rpc: AuditRpc,
    fake: Arc<Fake>,
    notifications: Arc<std::sync::Mutex<Vec<AuditProgress>>>,
}

#[tokio::test]
async fn delete_preserves_source_other_runs_and_refuses_running_or_invalid_ids() -> Result<()> {
    let f = Fixture::new()?;
    let source = f.home.path().join("source");
    std::fs::create_dir(&source)?;
    std::fs::write(source.join("example.rs"), "preserve live source\n")?;
    let input = json!({"type":"paths","paths":[source]});
    let first = f.start(input.clone(), &["good"]).await?;
    let second = f.start(input.clone(), &["good"]).await?;
    f.wait(&first).await?;
    f.wait(&second).await?;
    let second_before = f
        .request(
            "audit/read",
            json!({"runId":second,"view":"output","reviewer":"1"}),
        )
        .await?;
    let directory = f.home.path().join("audex/runs").join(&first);
    #[cfg(unix)]
    std::os::unix::fs::symlink(&source, directory.join("source-link"))?;
    assert!(
        f.request("audit/delete", json!({"runId":"../../source"}))
            .await
            .is_err()
    );
    assert_eq!(
        f.request("audit/delete", json!({"runId":first})).await?["runId"],
        first
    );
    assert!(!directory.exists());
    assert_eq!(
        std::fs::read_to_string(source.join("example.rs"))?,
        "preserve live source\n"
    );
    assert_eq!(
        f.request(
            "audit/read",
            json!({"runId":second,"view":"output","reviewer":"1"})
        )
        .await?,
        second_before
    );
    let page = f.request("audit/list", json!({})).await?;
    assert_eq!(page["runs"].as_array().unwrap().len(), 1);
    assert_eq!(page["runs"][0]["runId"], second);
    assert!(
        f.request("audit/delete", json!({"runId":first}))
            .await
            .is_err()
    );
    let running = f.start(input, &["wait"]).await?;
    assert!(
        f.request("audit/delete", json!({"runId":running}))
            .await
            .is_err()
    );
    f.request("audit/cancel", json!({"runId":running})).await?;
    f.wait(&running).await?;
    f.request("audit/delete", json!({"runId":running})).await?;
    Ok(())
}
impl Fixture {
    fn new() -> Result<Self> {
        let home = tempfile::tempdir()?;
        let fake = Arc::new(Fake {
            calls: AtomicUsize::new(0),
            entered: Semaphore::new(0),
        });
        let notifications = Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = notifications.clone();
        let root = home.path().join("audex/runs");
        let service = AuditService::open(
            home.path(),
            fake.clone(),
            Arc::new(move |e| captured.lock().unwrap().push(progress(&root, e))),
        )?;
        let mut settings = BackendSettings::default();
        settings
            .presets
            .insert("default".into(), vec!["native:chosen:high".into()]);
        let rpc = AuditRpc {
            service: Mutex::new(Ok(service)),
            settings,
            stopped: AtomicBool::new(false),
        };
        Ok(Self {
            home,
            rpc,
            fake,
            notifications,
        })
    }
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let request: ClientRequest =
            serde_json::from_value(json!({"id":1,"method":method,"params":params}))?;
        let result = self
            .rpc
            .handle(request)
            .await
            .map_err(|e| anyhow::anyhow!("{}", e.message))?;
        Ok(serde_json::to_value(result)?)
    }
    async fn start(&self, input: Value, models: &[&str]) -> Result<String> {
        let result = self.request("audit/start", json!({"input":input,"cwd":self.home.path(),"brief":"brief","reviewers":models.iter().map(|m| json!({"route":"native","model":m,"effort":"high"})).collect::<Vec<_>>()})).await?;
        Ok(result["run"]["runId"].as_str().context("run id")?.into())
    }
    async fn wait(&self, id: &str) -> Result<()> {
        self.rpc
            .service
            .lock()
            .await
            .as_mut()
            .unwrap()
            .wait(uuid::Uuid::parse_str(id)?)
            .await
    }
}

#[tokio::test]
async fn rpc_uses_run_files_for_all_shapes_guidance_and_all_read_views() -> Result<()> {
    let f = Fixture::new()?;
    let first = f.home.path().join("first");
    let second = f.home.path().join("second");
    std::fs::create_dir(&first)?;
    std::fs::create_dir(&second)?;
    std::fs::write(first.join("example.rs"), "first\n")?;
    std::fs::write(second.join("example.rs"), "second\n")?;
    let shapes = [
        json!({"type":"paths","paths":[first]}),
        json!({"type":"paths","paths":[first,second]}),
        json!({"type":"paths","paths":[first.join("example.rs"),second.join("example.rs")]}),
        json!({"type":"snippet","name":"example.rs","text":"pasted\n"}),
    ];
    for input in shapes {
        let id = f.start(input.clone(), &["good"]).await?;
        f.wait(&id).await?;
        let run = f.request("audit/read", json!({"runId":id})).await?;
        assert_eq!(run["run"]["complete"], true);
        assert!(
            run["run"]["summary"]
                .as_str()
                .unwrap()
                .contains("fake review")
        );
        let directory = f.home.path().join("audex/runs").join(&id);
        let summary_path = directory.join("summary.md");
        let original = std::fs::read_to_string(&summary_path)?;
        assert!(!original.contains("may have changed"));
        let legacy = format!("{original}\nSource is live and may have changed since the review.\n");
        std::fs::write(&summary_path, &legacy)?;
        let reread = f.request("audit/read", json!({"runId":id})).await?;
        assert_eq!(reread["run"]["summary"], original);
        assert_eq!(std::fs::read_to_string(&summary_path)?, legacy);
        let record = service::read::record(&directory)?;
        if input["type"] == "paths" {
            assert_eq!(
                record.targets.paths.len(),
                input["paths"].as_array().unwrap().len()
            );
        }
        assert_eq!(record.reviewers[0].effort.as_deref(), Some("high"));
        for view in ["reviewers", "findings", "finding", "diagnostics"] {
            let read = f
                .request(
                    "audit/read",
                    json!({"runId":id,"view":view,"reviewer":"1","number":1,"offset":0,"limit":1}),
                )
                .await?;
            assert_eq!(read["data"]["items"].as_array().unwrap().len(), 1);
            assert!(read["data"].get("sourceNotice").is_none());
            assert_eq!(read["run"]["sourceNotice"], "");
            assert!(!read.to_string().contains("may have changed"));
            if view == "findings" {
                assert_eq!(read["data"]["nextOffset"], 1);
            }
            if view == "diagnostics" {
                let item = &read["data"]["items"][0];
                assert_eq!(item["toolCallSummary"]["count"], 4);
                assert!(item["stderrTail"].as_str().unwrap().contains("sentinel"));
                assert!(
                    item["transcriptPath"]
                        .as_str()
                        .unwrap()
                        .ends_with("transcript.jsonl")
                );
            }
        }
        let mut output = String::new();
        let mut offset = 0;
        loop {
            let read = f
                .request(
                    "audit/read",
                    json!({"runId":id,"view":"output","reviewer":"1","offset":offset,"limit":2}),
                )
                .await?;
            output.push_str(read["data"]["content"].as_str().unwrap());
            let Some(next) = read["data"]["nextOffset"].as_u64() else {
                break;
            };
            offset = next;
        }
        assert_eq!(output, "literal é漢🙂\n");
    }
    std::fs::write(f.home.path().join("AUDEX.md"), "guidance sentinel")?;
    let start=f.request("audit/start",json!({"input":{"type":"paths","paths":[first]},"cwd":f.home.path(),"brief":"brief","guidance":"AUDEX.md","reviewers":[{"route":"native","model":"good"}]})).await?;
    let id = start["run"]["runId"].as_str().unwrap();
    f.wait(id).await?;
    std::fs::write(f.home.path().join("AUDEX.md"), "changed")?;
    let record = service::read::record(&f.home.path().join("audex/runs").join(id))?;
    assert_eq!(record.guidance.as_deref(), Some("guidance sentinel"));
    assert_eq!(record.brief, "brief");
    assert_eq!(
        serde_json::to_string(&record)?
            .matches("guidance sentinel")
            .count(),
        1
    );
    let page = f.request("audit/list", json!({"limit":1})).await?;
    assert_eq!(page["runs"].as_array().unwrap().len(), 1);
    assert_eq!(page["nextOffset"], 1);
    assert_eq!(page["presets"]["default"][0], "native:chosen:high");
    assert!(!f.home.path().join("audex/snapshots").exists());
    assert!(
        std::fs::read_dir(f.home.path().join("audex"))?.all(|e| e.unwrap().file_name() == "runs")
    );
    assert!(f.notifications.lock().unwrap().iter().any(|e| e.complete));
    Ok(())
}

#[tokio::test]
async fn inline_guidance_is_literal_and_conflicting_sources_never_start_a_run() -> Result<()> {
    let f = Fixture::new()?;
    let filename = "review notes.md";
    std::fs::write(f.home.path().join(filename), "guidance from file")?;
    let mut params = json!({
        "input": {"type":"snippet", "name":"example.rs", "text":"source"},
        "cwd": f.home.path(), "brief":"review", "reviewers":[{"route":"native","model":"good"}],
        "guidance":"missing file", "guidanceText": filename,
    });
    let error = f.request("audit/start", params.clone()).await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("only one of guidance and guidanceText")
    );
    assert_eq!(f.request("audit/list", json!({})).await?["runs"], json!([]));
    assert_eq!(f.fake.calls.load(Ordering::SeqCst), 0);
    params.as_object_mut().unwrap().remove("guidance");
    for expected in [filename, "guidance from file"] {
        let response = f.request("audit/start", params.clone()).await?;
        let id = response["run"]["runId"].as_str().context("run id")?;
        f.wait(id).await?;
        let record = service::read::record(&f.home.path().join("audex/runs").join(id))?;
        assert_eq!(record.guidance.as_deref(), Some(expected));
        params.as_object_mut().unwrap().remove("guidanceText");
        params["guidance"] = json!(filename);
    }
    Ok(())
}

#[tokio::test]
async fn s4_f4_listing_skips_only_missing_records_and_keeps_presets() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut f = Fixture::new()?;
    f.rpc
        .settings
        .presets
        .insert("named".into(), vec!["native:named:low".into()]);
    let id = f
        .start(
            json!({"type":"snippet","name":"example.rs","text":"source"}),
            &["good"],
        )
        .await?;
    f.wait(&id).await?;
    let root = f.home.path().join("audex/runs");
    let incomplete = root.join(uuid::Uuid::now_v7().to_string());
    std::fs::create_dir(&incomplete)?;
    let page = f.request("audit/list", json!({"limit":1})).await?;
    assert_eq!(page["runs"].as_array().unwrap().len(), 1);
    assert_eq!(page["runs"][0]["runId"], id);
    assert!(page["nextOffset"].is_null());
    assert_eq!(page["presets"]["default"][0], "native:chosen:high");
    assert_eq!(page["presets"]["named"][0], "native:named:low");
    assert!(
        incomplete.is_dir(),
        "listing must not repair or remove the directory"
    );
    std::fs::write(incomplete.join("run.json"), "malformed record")?;
    assert!(f.request("audit/list", json!({})).await.is_err());
    std::fs::remove_file(incomplete.join("run.json"))?;
    let record = root.join(&id).join("run.json");
    std::fs::set_permissions(&record, std::fs::Permissions::from_mode(0o000))?;
    let unreadable = f.request("audit/list", json!({})).await;
    std::fs::set_permissions(&record, std::fs::Permissions::from_mode(0o600))?;
    assert!(
        unreadable.is_err(),
        "unreadable records must still fail listing"
    );
    Ok(())
}

#[tokio::test]
async fn rpc_cancel_retry_zero_output_replacement_and_retained_attempts() -> Result<()> {
    let f = Fixture::new()?;
    let id = f
        .start(
            json!({"type":"snippet","name":"example.rs","text":"source"}),
            &["zero", "wait"],
        )
        .await?;
    let permit = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        f.fake.entered.acquire_many(3),
    )
    .await??;
    permit.forget();
    f.request("audit/cancel", json!({"runId":id,"reviewer":"2"}))
        .await?;
    f.wait(&id).await?;
    let read = f.request("audit/read", json!({"runId":id})).await?;
    let statuses = read["run"]["reviewers"].as_array().unwrap();
    assert_eq!(statuses.len(), 3);
    assert_eq!(statuses[1]["trigger"], "automatic_replacement");
    assert_eq!(statuses[2]["state"], "cancelled");
    let partial = f
        .request(
            "audit/read",
            json!({"runId":id,"view":"output","reviewer":"2"}),
        )
        .await?;
    assert_eq!(partial["data"]["content"], "partial é漢🙂");
    f.request("audit/retry", json!({"runId":id,"reviewer":"1"}))
        .await?;
    f.wait(&id).await?;
    let read = f.request("audit/read", json!({"runId":id})).await?;
    assert_eq!(read["run"]["reviewers"][2]["trigger"], "operator_retry");
    assert_eq!(f.fake.calls.load(Ordering::SeqCst), 4);
    assert!(
        f.request("audit/read", json!({"runId":id,"view":"source"}))
            .await
            .is_err()
    );
    for method in [
        "audit/preflight",
        "audit/evidence",
        "audit/attempts",
        "audit/attempt/read",
        "audit/attempt/control",
        "audit/release-slot",
        "audit/capabilities",
    ] {
        assert!(
            serde_json::from_value::<ClientRequest>(json!({"id":1,"method":method,"params":{}}))
                .is_err()
        );
    }
    Ok(())
}

#[tokio::test]
async fn rpc_start_accepts_unlistable_directory_and_restart_interrupts_without_dispatch()
-> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new()?;
    let target = f.home.path().join("unlistable");
    std::fs::create_dir(&target)?;
    std::fs::write(target.join("example.rs"), "source\n")?;
    struct Restore(std::path::PathBuf);
    impl Drop for Restore {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o700));
        }
    }
    let restore = Restore(target.clone());
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o111))?;
    assert!(std::fs::read_dir(&target).is_err());
    let id = f
        .start(json!({"type":"paths","paths":[target]}), &["wait"])
        .await?;
    f.fake.entered.acquire().await?.forget();
    let mut guard = f.rpc.service.lock().await;
    *guard = Err(anyhow::anyhow!("simulate process shutdown"));
    *guard = Ok(AuditService::open(
        f.home.path(),
        f.fake.clone(),
        Arc::new(|_| {}),
    )?);
    drop(guard);
    let read = f.request("audit/read", json!({"runId":id})).await?;
    assert_eq!(read["run"]["reviewers"][0]["state"], "interrupted");
    assert_eq!(read["run"]["reviewers"][0]["reason"], "Audex restarted");
    assert!(read["run"]["complete"].as_bool().unwrap());
    assert_eq!(f.fake.calls.load(Ordering::SeqCst), 1);
    drop(restore);
    Ok(())
}
