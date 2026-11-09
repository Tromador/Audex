//! Complete retained findings through the normal managed-daemon conversation path.
use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

#[path = "../../../../cli/tests/app_server_daemon/support.rs"]
mod daemon_support;

#[test]
fn managed_conversation_compares_complete_run_findings_without_individual_reads()
-> anyhow::Result<()> {
    run_test_with_large_stack("audit-run-findings", || exercise(false, false))
}

#[test]
fn managed_conversation_compares_complete_reviewer_collections_without_individual_reads()
-> anyhow::Result<()> {
    run_test_with_large_stack("audit-reviewer-findings", || exercise(true, false))
}

fn claims(model: &str) -> Vec<Value> {
    let count = if model == "fixture-luna" { 1 } else { 2 };
    (0..count).map(|n| json!({"severity":"high","confidence":"medium",
        "title":format!("{model} finding {}",n+1),"mechanism":format!("{model} mechanism {n}: β complete claim"),
        "impact":format!("{model} impact {n}"),"remediation":format!("{model} remediation {n}"),
        "evidence":[{"description":format!("{model} evidence {n}"),"path":"example.rs",
            "start_byte":0,"end_byte":2,"start_line":1,"end_line":1}]})).collect()
}

fn check_finding(item: &Value, model: &str, index: usize) {
    let original = &claims(model)[index];
    for key in [
        "title",
        "severity",
        "confidence",
        "mechanism",
        "impact",
        "remediation",
    ] {
        assert_eq!(item[key], original[key], "{key}");
    }
    assert!(item["id"].as_str().is_some_and(|id| !id.is_empty()));
    assert_eq!(item["ordinal"], index);
    assert_eq!(item["verification_state"], "unverified_model_claim");
    assert_eq!(
        item["evidence"],
        json!([{"ordinal":0,"description":original["evidence"][0]["description"],
        "manifest_path":"example.rs","start_byte":0,"end_byte":2,"start_line":1,"end_line":1,"lines_verified":true}])
    );
}

fn output(body: &Value, id: &str) -> Value {
    let item = body["input"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["type"] == "function_call_output" && item["call_id"] == id)
        .expect("retrieval output reaches the ordinary conversation");
    nested_object(item, "identity").expect("structured evidence output")
}

async fn exercise(per_reviewer: bool, automatic: bool) -> anyhow::Result<()> {
    let provider = responses::start_mock_server().await;
    let daemon = daemon_support::TestDaemon::new()?;
    let home = daemon.home.path();
    app_test_support::MockResponsesConfig::new(&provider.uri())
        .with_model("audit-test")
        .disable_feature(Feature::Plugins)
        .disable_feature(Feature::RemotePlugin)
        .disable_feature(Feature::PluginDiscoveryPrefetch)
        .disable_feature(Feature::CodeMode)
        .write(home)?;
    let config = ConfigBuilder::default()
        .codex_home(home.to_owned())
        .harness_overrides(ConfigOverrides {
            cwd: Some(home.to_owned()),
            ..Default::default()
        })
        .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
        .build()
        .await?;
    let source = home.join("example.rs");
    std::fs::write(&source, "fn main() {}\n")?;
    let reviewer_calls = Arc::new(AtomicUsize::new(0));
    let observed = reviewer_calls.clone();
    Mock::given(method("POST")).and(path("/v1/responses"))
        .and(|r: &wiremock::Request| String::from_utf8_lossy(&r.body).contains("audex_source"))
        .respond_with(move |r: &wiremock::Request| {
            observed.fetch_add(1, Ordering::SeqCst);
            let body: Value = serde_json::from_slice(&r.body).unwrap();
            let input = body["input"].as_array().unwrap();
            let assignment = input.iter().filter_map(|i| i["content"].as_array()).flatten()
                .filter_map(|p| p["text"].as_str()).filter_map(|t| serde_json::from_str::<Value>(t).ok())
                .find(|v| v.get("assignment_id").is_some()).unwrap();
            let events = if input.iter().any(|i| i["type"] == "function_call_output") {
                let result = json!({"schema_version":1,"snapshot_id":assignment["snapshot_id"],"assignment_id":assignment["assignment_id"],
                    "summary":"Independent fixture claims","findings":claims(body["model"].as_str().unwrap())});
                vec![responses::ev_assistant_message("result", &result.to_string()), responses::ev_completed("review-final")]
            } else {
                vec![responses::ev_function_call_with_namespace("source-read", "audex_source", "read",
                    r#"{"path":"example.rs","offset":0,"max_bytes":64}"#), responses::ev_completed("review-read")]
            };
            ResponseTemplate::new(200).insert_header("content-type", "text/event-stream")
                .insert_header("OpenAI-Model",body["model"].as_str().unwrap()).set_body_string(responses::sse(events))
        }).mount(&provider).await;
    let reference = Arc::new(std::sync::Mutex::new(Value::Null));
    let model_reference = reference.clone();
    let operator_calls = Arc::new(AtomicUsize::new(0));
    let observed = operator_calls.clone();
    Mock::given(|r: &wiremock::Request| {
        let text = String::from_utf8_lossy(&r.body);
        text.contains("FINDING_COLLECTION_FOLLOWUP") || text.contains("Audex automatic completion comparison")
    })
        .respond_with(move |r: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&r.body).unwrap();
            let index = observed.fetch_add(1, Ordering::SeqCst);
            let run = model_reference.lock().unwrap().clone();
            let collection_calls = if per_reviewer { 3 } else { 1 };
            let request = if index < collection_calls {
                if !per_reviewer {
                    Some(("all-findings".to_owned(), "audit_read", json!({"runId":run["runId"],"view":"findings"})))
                } else if index == 0 {
                    Some(("roster".to_owned(), "audit_read", json!({"runId":run["runId"]})))
                } else {
                    let roster = output(&body, "roster");
                    Some((format!("reviewer-{index}"), "audit_read", json!({"runId":run["runId"],"view":"findings","reviewer":roster["items"][index-1]["reviewerId"]})))
                }
            } else if index == collection_calls {
                let collections = if per_reviewer { vec![output(&body, "reviewer-1"), output(&body, "reviewer-2")] }
                    else { vec![output(&body, "all-findings")] };
                let mut found = 0;
                for page in collections {
                    assert_eq!(page["identity"]["runId"],run["runId"]);
                    assert_eq!(page["identity"]["snapshotId"],run["snapshotId"]);
                    assert!(page["identity"]["manifestDigest"].as_str().is_some_and(|s| s.len()==64));
                    assert!(page["nextOffset"].is_null());
                    for item in page["items"].as_array().unwrap() {
                        let attribution = if per_reviewer { &page["reviewer"] } else {
                            page["reviewers"].as_array().unwrap().iter().find(|reviewer| reviewer["reviewerId"]==item["reviewerId"]).unwrap()
                        };
                        assert_eq!(item["assignmentId"],attribution["assignmentId"]);
                        check_finding(item,attribution["model"].as_str().unwrap(),item["number"].as_u64().unwrap() as usize-1);
                        found += 1;
                    }
                }
                assert_eq!(found,3,"all complete claims before any individual finding request");
                Some(("frozen-source".to_owned(),"audit_source",json!({"runId":run["runId"],"snapshot":run["snapshotId"],"path":"example.rs"})))
            } else {
                assert_eq!(index, collection_calls+1);
                assert_eq!(output(&body,"frozen-source")["content"],"fn main() {}\n");
                assert!(!body["input"].to_string().contains("CHANGED_WORKING_COPY"));
                None
            };
            let events = if let Some((id,suffix,args)) = request {
                let (namespace,tool) = named_tool(&body["tools"],suffix).expect("production managed-daemon gate");
                vec![responses::ev_function_call_with_namespace(&id,&namespace,&tool,&args.to_string()),responses::ev_completed(&id)]
            } else {
                vec![responses::ev_assistant_message("compared","Compared all three retained claims against their captured source."),responses::ev_completed("comparison-complete")]
            };
            ResponseTemplate::new(200).insert_header("content-type","text/event-stream").insert_header("OpenAI-Model","audit-test").set_body_string(responses::sse(events))
        }).with_priority(1).mount(&provider).await;
    daemon.lifecycle("start")?;
    let socket = codex_app_server_client::app_server_control_socket_path(home)?;
    let endpoint = codex_app_server_client::RemoteAppServerEndpoint::UnixSocket {
        socket_path: codex_app_server_client::verified_local_daemon_socket(socket.as_path())?
            .unwrap(),
    };
    let target = crate::AppServerTarget::LocalDaemon {
        endpoint: endpoint.clone(),
        allow_embedded_fallback: false,
    };
    let client = crate::connect_remote_app_server(endpoint)
        .await
        .map_err(|e| anyhow::anyhow!("{e:#}"))?;
    let mut session = AppServerSession::new(
        crate::backend_id::BackendId::new(),
        client,
        target.thread_params_mode(),
    )
    .with_local_codex_home(&config.codex_home);
    let (tx, _events) = mpsc::unbounded_channel();
    session
        .start_dynamic_tool_mcp(
            config.clone(),
            AppEventSender::new(tx),
            tokio::sync::broadcast::channel(16).0,
        )
        .await?;
    let thread = boxed_future!(session.start_thread(&config))
        .await
        .map_err(|e| anyhow::anyhow!("{e:#}"))?
        .session
        .thread_id;
    let audits = AuditClient::new(session.request_handle());
    let started = audits.start(serde_json::from_value(json!({"requestKey":"complete-findings-fixture",
        "source":{"type":"file","name":"example.rs","path":source},"brief":"fixture",
        "reviewers":[{"provider":"mock_provider","model":"fixture-luna"},{"provider":"mock_provider","model":"fixture-haiku"}]}))?).await?;
    let retained = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let run = audits.read(started.run_id.clone()).await.unwrap();
            if run.report.is_some() {
                break run;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?;
    assert_eq!(retained.phase, AuditRunPhase::Completed);
    assert_eq!(reviewer_calls.load(Ordering::SeqCst), 4);
    std::fs::write(&source, "CHANGED_WORKING_COPY")?;
    *reference.lock().unwrap() = json!({"runId":retained.run_id,"snapshotId":retained.snapshot_id});
    let mut contexts = ReportContexts::default();
    contexts.stage(thread, &retained);
    let prompt = if automatic {
        comparison_prompt(&retained.run_id, &retained.snapshot_id)
    } else {
        "FINDING_COLLECTION_FOLLOWUP: Where do these reports agree?".into()
    };
    if automatic {
        assert!(
            retained
                .findings_summary
                .as_ref()
                .unwrap()
                .contains("fixture-haiku")
        );
        assert!(
            retained
                .findings_summary
                .as_ref()
                .unwrap()
                .contains("fixture-luna")
        );
        assert!(
            !retained
                .findings_summary
                .as_ref()
                .unwrap()
                .contains("Policy digest")
        );
    }
    turn(
        &mut session,
        thread,
        &prompt,
        &config,
        contexts.for_turn(thread),
    )
    .await
    .map_err(|e| anyhow::anyhow!("{e:#}"))?;
    let mut reads = 0;
    let mut sources = 0;
    tokio::time::timeout(Duration::from_secs(30), async {
        while let Some(event) = session.next_event().await {
            if let AppServerEvent::ServerNotification(notification) = event.event {
                match notification.as_ref() {
                    ServerNotification::ItemCompleted(done)
                        if done.thread_id == thread.to_string() =>
                    {
                        let item = serde_json::to_value(&done.item).unwrap();
                        if let Some(tool) = item["tool"].as_str() {
                            if tool.ends_with("audit_read") {
                                reads += 1;
                                assert_ne!(item["arguments"]["view"], "finding");
                            }
                            if tool.ends_with("audit_source") {
                                sources += 1;
                            }
                        }
                    }
                    ServerNotification::TurnCompleted(done)
                        if done.thread_id == thread.to_string() =>
                    {
                        break;
                    }
                    _ => {}
                }
            }
        }
    })
    .await?;
    assert_eq!(reads, if per_reviewer { 3 } else { 1 });
    assert_eq!(sources, 1);
    assert_eq!(operator_calls.load(Ordering::SeqCst), reads + 2);
    assert_eq!(
        reviewer_calls.load(Ordering::SeqCst),
        4,
        "follow-up starts no reviewer generation"
    );
    if automatic {
        let (tx, mut events) = mpsc::unbounded_channel();
        let full = execute(
            session.request_handle(),
            thread,
            AuditAction::Diagnostics(
                DiagnosticArguments {
                    run_id: retained.run_id.clone(),
                    offset: 0,
                    max_bytes: None,
                },
                true,
            ),
            AppEventSender::new(tx),
        )
        .await
        .map_err(anyhow::Error::msg)?;
        assert_eq!(full["content"], *retained.report.as_ref().unwrap());
        assert_eq!(full["eof"], true);
        assert!(matches!(
            events.recv().await,
            Some(AppEvent::AuditDiagnostics { .. })
        ));
        assert_eq!(reviewer_calls.load(Ordering::SeqCst), 4);
    }
    assert_eq!(
        audits.read(retained.run_id).await?.revision,
        retained.revision
    );
    assert_eq!(std::fs::read_to_string(&source)?, "CHANGED_WORKING_COPY");
    if let Some(root) = std::env::var_os("AUDEX_S5_EVIDENCE") {
        std::fs::write(
            std::path::PathBuf::from(root).join(if per_reviewer {
                "findings-reviewer.json"
            } else {
                "findings-run.json"
            }),
            serde_json::to_vec_pretty(
                &json!({"perReviewer":per_reviewer,"completeFindings":3,"auditReadCalls":reads,"individualFindingCalls":0,
                "sourceCalls":sources,"reviewerProviderRequests":4,"operatorProviderRequests":reads+2,"changedWorkingCopyIgnored":true}),
            )?,
        )?;
    }
    session.shutdown().await?;
    Ok(())
}

#[test]
fn managed_automatic_comparison_prompt_retrieves_evidence_and_explicit_diagnostics()
-> anyhow::Result<()> {
    run_test_with_large_stack("audit-automatic-comparison", || exercise(false, true))
}
