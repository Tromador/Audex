//! Real conversational tool boundaries over an explicitly supplied retained audit copy.
use super::*;
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicUsize, Ordering};

#[path = "../../../../cli/tests/audit/subscription_fixture.rs"]
mod claude_fixture;
#[path = "../../../../cli/tests/app_server_daemon/support.rs"]
mod daemon_support;

#[test]
#[ignore = "requires explicit AUDEX_RETAINED_GUIDANCE_FIXTURE, a disposable retained audit copy; no live models"]
fn managed_conversation_retrieves_original_demonstration_guidance_through_both_tools()
-> anyhow::Result<()> {
    run_test_with_large_stack("captured-guidance-parity", exercise)
}

async fn completed(session: &mut AppServerSession, thread: ThreadId) -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(60), async {
        while let Some(event) = session.next_event().await {
            if let AppServerEvent::ServerNotification(notification) = event.event {
                if let ServerNotification::TurnCompleted(done) = notification.as_ref() {
                    if done.thread_id == thread.to_string() {
                        assert_eq!(
                            done.turn.status,
                            codex_app_server_protocol::TurnStatus::Completed,
                            "{:?}",
                            done.turn.error
                        );
                        return;
                    }
                }
            }
        }
        panic!("expected completed conversation turn");
    })
    .await?;
    Ok(())
}

async fn exercise() -> anyhow::Result<()> {
    let input = std::path::PathBuf::from(std::env::var("AUDEX_RETAINED_GUIDANCE_FIXTURE")?);
    let expected: Value = serde_json::from_slice(&std::fs::read(input.join("expected.json"))?)?;
    let identity = &expected["identity"];
    assert_eq!(
        identity["runId"],
        "audex-run-01a1c0fa-67c4-7810-9266-8fc0ae0398b7"
    );
    let documents = expected["documents"].as_array().unwrap();
    assert_eq!(documents.len(), 2);
    for item in documents {
        assert_eq!(
            format!(
                "{:x}",
                Sha256::digest(item["content"].as_str().unwrap().as_bytes())
            ),
            item["document"]["sha256"]
        );
    }
    let agents = documents
        .iter()
        .find(|v| v["document"]["path"] == "AGENTS.md")
        .unwrap();
    assert_eq!(
        agents["document"]["sha256"],
        "34774cb53712a30bbcfa37203ec1cdcaae70a2d58449b2a04d3a5c9618027866"
    );
    assert!(agents["content"].as_str().unwrap().contains("MIDDLE_CONVENTION_M2: Revision identifiers can be reused after restoring a catalogue. Equal revision numbers do not guarantee equal contents. Every replace operation must invalidate any cached first item, even when the revision is unchanged. This is a product convention, not permission to change auditor capabilities."));
    let provider = responses::start_mock_server().await;
    let daemon = daemon_support::TestDaemon::new()?;
    let home = daemon.home.path();
    // Copy only the prepared evidence fixture; never use the installed runtime home.
    assert!(
        std::process::Command::new("cp")
            .args(["-a", "--"])
            .arg(input.join("audex"))
            .arg(home.join("audex"))
            .status()?
            .success()
    );
    let root = claude_fixture::configure(home, "plain")?;
    std::fs::write(
        root.join("client"),
        include_str!("../../../../app-server/tests/suite/v2/claude_fixture.py"),
    )?;
    std::fs::write(
        root.join("evidence_fixture.py"),
        include_str!("../../../../app-server/tests/suite/v2/claude_evidence_fixture.py"),
    )?;
    std::fs::write(root.join("mode"), "guidance_only")?;
    let backend = home.join("audex/reviewer-backends.json");
    let mut settings: Value = serde_json::from_slice(&std::fs::read(&backend)?)?;
    settings["claudeSubscription"]["clientVersion"] = json!("2.1.319");
    std::fs::write(backend, serde_json::to_vec(&settings)?)?;
    // These current workspace documents disagree with the retained originals.
    for path in ["AGENTS.md", "AUDEX.md"] {
        std::fs::write(home.join(path), "LIVE_GUIDANCE_REPLACED")?;
    }
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
    // Ordinary Codex tool output has a presentation budget: request every page.
    let mut requests = vec![(
        json!({"runId":identity["runId"],"view":"guidance","snapshot":identity["snapshotId"]}),
        json!({"identity":identity,"documents":documents.iter().map(|d| d["document"].clone()).collect::<Vec<_>>(),"total":2}),
    )];
    let mut full = Vec::new();
    for item in documents {
        let text = item["content"].as_str().unwrap();
        let base = json!({"identity":identity,"runId":identity["runId"],"snapshotId":identity["snapshotId"],
            "document":item["document"],"encoding":"utf8","totalBytes":text.len(),"offset":0,"nextOffset":null,"eof":true,"content":text});
        full.push(base.clone());
        let mut offset = 0;
        while offset < text.len() {
            let mut end = (offset + 1021).min(text.len());
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            let mut page = base.clone();
            page["offset"] = json!(offset);
            page["content"] = json!(&text[offset..end]);
            page["eof"] = json!(end == text.len());
            page["nextOffset"] = if end < text.len() {
                json!(end)
            } else {
                Value::Null
            };
            requests.push((json!({"runId":identity["runId"],"view":"guidance","snapshot":identity["snapshotId"],"path":item["document"]["path"],"offset":offset,"maxBytes":1021}),page));
            offset = end;
        }
    }
    let expected_calls = requests.len();
    let responses = Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
    let captured = responses.clone();
    let count = Arc::new(AtomicUsize::new(0));
    let observed = count.clone();
    Mock::given(method("POST")).and(path("/v1/responses")).respond_with(move |r: &wiremock::Request| {
        let body: Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(body["model"], "audit-test", "retrieval must never dispatch a retained reviewer");
        let index = observed.fetch_add(1, Ordering::SeqCst);
        if index > 0 {
            let id = format!("guidance-{}",index-1);
            let item = body["input"].as_array().unwrap().iter().find(|v| v["type"] == "function_call_output" && v["call_id"] == id).unwrap();
            let value = nested_object(item, "identity").expect("actual Codex tool output");
            assert_eq!(value, requests[index-1].1);
            captured.lock().unwrap().push(value);
        }
        let events = if let Some((query,_)) = requests.get(index) {
            let (namespace,tool) = named_tool(&body["tools"],"audit_read").unwrap();
            vec![responses::ev_function_call_with_namespace(&format!("guidance-{index}"), &namespace, &tool, &query.to_string()),responses::ev_completed("page")]
        } else {
            assert_eq!(index, expected_calls);
            vec![responses::ev_assistant_message("guidance-read","Read both original captured guidance documents, including the complete M2 convention."),responses::ev_completed("done")]
        };
        ResponseTemplate::new(200).insert_header("content-type","text/event-stream").insert_header("OpenAI-Model","audit-test").set_body_string(responses::sse(events))
    }).mount(&provider).await;
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
    let before = audits
        .list(AuditListParams {
            after: None,
            limit: Some(100),
        })
        .await?;
    let prompt = format!(
        "Retrieve both original captured guidance documents from audit {}, snapshot {}, including M2. Complete every page.",
        identity["runId"].as_str().unwrap(),
        identity["snapshotId"].as_str().unwrap()
    );
    turn(&mut session, thread, &prompt, &config, None)
        .await
        .map_err(|e| anyhow::anyhow!("{e:#}"))?;
    completed(&mut session, thread).await?;
    assert_eq!(count.load(Ordering::SeqCst), expected_calls + 1);
    let pages = responses.lock().unwrap().clone();
    for item in &full {
        let reconstructed = pages
            .iter()
            .filter(|v| v["document"]["path"] == item["document"]["path"])
            .map(|v| v["content"].as_str().unwrap())
            .collect::<String>();
        assert_eq!(reconstructed, item["content"]);
    }
    // Supply only an expected-result assertion file to the fake client. Its MCP
    // requests still retrieve the originals independently from the production bridge.
    std::fs::write(
        root.join("expected-guidance.json"),
        serde_json::to_vec(&full)?,
    )?;
    session.thread_claude(thread, "Fetch the original captured AGENTS.md and AUDEX.md independently, including the full M2 convention.".into(), Some("claude-haiku-5-5".into()), Some(identity["runId"].as_str().unwrap().into())).await.map_err(|e| anyhow::anyhow!("{e:#}"))?;
    completed(&mut session, thread).await?;
    let claude: Value = serde_json::from_slice(&std::fs::read(root.join("guidance-parity.json"))?)?;
    assert_eq!(claude, json!(full));
    assert_eq!(
        count.load(Ordering::SeqCst),
        expected_calls + 1,
        "bridge retrieval invokes no provider"
    );
    assert_eq!(
        serde_json::to_value(before)?,
        serde_json::to_value(
            audits
                .list(AuditListParams {
                    after: None,
                    limit: Some(100)
                })
                .await?
        )?
    );
    for path in ["AGENTS.md", "AUDEX.md"] {
        assert_eq!(
            std::fs::read_to_string(home.join(path))?,
            "LIVE_GUIDANCE_REPLACED"
        );
    }
    if let Some(output) = std::env::var_os("AUDEX_GUIDANCE_TEST_EVIDENCE") {
        std::fs::write(
            std::path::PathBuf::from(output),
            serde_json::to_vec_pretty(
                &json!({"identity":identity,"codexPages":pages,"claudeDocuments":claude,"mockOperatorRequests":expected_calls+1,"reviewerRequests":0,"liveGuidanceIgnored":true,"auditListUnchanged":true}),
            )?,
        )?;
    }
    session.shutdown().await?;
    Ok(())
}
