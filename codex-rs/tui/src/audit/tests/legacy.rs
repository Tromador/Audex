//! Main-TUI legacy replay still confirms, then retrieves the original audit over daemon IPC.
use super::*;
#[path = "../../../../cli/tests/audit/legacy_fixture.rs"]
mod fixture;

#[test]
fn legacy_tui_confirmation_and_replay_after_restart_do_not_execute() -> anyhow::Result<()> {
    core_test_support::test_codex::run_test_with_large_stack("legacy-tui-replay", || async {
        scenario().await.map_err(|e| anyhow::anyhow!("{e:#}"))
    })
}

async fn scenario() -> Result<()> {
    let parent = std::env::var_os("AUDEX_TEST_TMPDIR").expect("explicit fixture parent");
    let daemon = support::TestDaemon::new_in(std::path::Path::new(&parent))
        .map_err(|e| color_eyre::eyre::eyre!("{e:#}"))?;
    let home = daemon.home.path();
    let native = responses::start_mock_server().await;
    Mock::given(method("POST")).and(path("/v1/responses")).respond_with(|request:&wiremock::Request| {
        let body:Value=request.body_json().unwrap();
        let assignment=body["input"].as_array().unwrap().iter().filter_map(|i|i["content"].as_array()).flatten()
            .filter_map(|p|p["text"].as_str()).filter_map(|s|serde_json::from_str::<Value>(s).ok())
            .find(|v|v.get("assignment_id").is_some()).unwrap();
        let result=json!({"schema_version":1,"snapshot_id":assignment["snapshot_id"],
            "assignment_id":assignment["assignment_id"],"summary":"Retained native review","findings":[]});
        ResponseTemplate::new(200).insert_header("content-type","text/event-stream").insert_header("OpenAI-Model","audit-test")
            .set_body_string(responses::sse(vec![responses::ev_response_created("native"),
                responses::ev_assistant_message("answer",&result.to_string()),responses::ev_completed("native")]))
    }).mount(&native).await;
    let config_path = home.join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"
model="audit-test"
model_provider="audit_fixture"
[features]
plugins=false
remote_plugin=false
plugin_discovery_prefetch=false
[model_providers.audit_fixture]
name="Owned legacy replay fixture"
base_url="{}/v1"
wire_api="responses"
request_max_retries=0
stream_max_retries=0
"#,
            native.uri()
        ),
    )?;
    std::fs::write(
        home.join("app-server-daemon/settings.json"),
        r#"{"shutdownGraceSeconds":0,"updater":{"autoUpdateEnabled":false}}"#,
    )?;
    daemon
        .lifecycle("start")
        .map_err(|e| color_eyre::eyre::eyre!("{e:#}"))?;
    let config = ConfigBuilder::default()
        .codex_home(home.to_owned())
        .harness_overrides(ConfigOverrides {
            cwd: Some(home.to_owned()),
            ..Default::default()
        })
        .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
        .build()
        .await?;
    let mut session = connect(home).await?;
    let thread = session.start_thread(&config).await?.session.thread_id;
    let args = || StartArguments {
        source: AuditSource::Snippet {
            name: "example.rs".into(),
            content: "fn main() {}\n".into(),
        },
        model: None,
        provider: None,
        reviewers: Some(
            (0..3)
                .map(|_| {
                    serde_json::from_value(json!({"provider":"audit_fixture","model":"audit-test"}))
                        .unwrap()
                })
                .collect(),
        ),
        scheduling: Default::default(),
        expected_served_model: None,
        request_key: Some("legacy-tui".into()),
        brief: "Original brief".into(),
        audex_guidance: Some("Original guidance".into()),
        agents_evidence: None,
    };
    let started = start_confirmed(&session, thread, args(), Some(true))
        .await?
        .unwrap();
    let audits = AuditClient::new(session.request_handle());
    let original = published(&audits, &started.run_id).await?;
    assert_eq!(original.phase, AuditRunPhase::Completed);
    let attempts = audits.attempts(original.run_id.clone()).await?.attempts;
    assert_eq!(attempts.len(), 3);
    assert_eq!(native.received_requests().await.unwrap().len(), 3);
    session.shutdown().await?;
    daemon
        .lifecycle("stop")
        .map_err(|e| color_eyre::eyre::eyre!("{e:#}"))?;
    fixture::retain_v1(home, &original.run_id).map_err(|e| color_eyre::eyre::eyre!("{e:#}"))?;
    std::fs::write(
        &config_path,
        std::fs::read_to_string(&config_path)?.replace("audit_fixture", "changed-provider"),
    )?;
    let before = fixture::fingerprint(home).map_err(|e| color_eyre::eyre::eyre!("{e:#}"))?;
    daemon
        .lifecycle("start")
        .map_err(|e| color_eyre::eyre::eyre!("{e:#}"))?;
    // Reopen from a fresh operator conversation after daemon restart. This loads
    // the thread/read context required by the TUI without any operator inference.
    let config = ConfigBuilder::default()
        .codex_home(home.to_owned())
        .harness_overrides(ConfigOverrides {
            cwd: Some(home.to_owned()),
            ..Default::default()
        })
        .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
        .build()
        .await?;
    let mut session = connect(home).await?;
    let thread = session.start_thread(&config).await?.session.thread_id;
    let mut disclosures = Vec::new();
    for decision in [Some(false), None, Some(true)] {
        let (events, mut receive) = sender();
        let task = tokio::spawn(execute(
            session.request_handle(),
            thread,
            AuditAction::Start(args()),
            events,
        ));
        let Some(event) = receive.recv().await else {
            return Err(color_eyre::eyre::eyre!(
                "Start ended before confirmation: {:?}",
                task.await?
            ));
        };
        let AppEvent::AuditConfirm {
            params,
            decision: answer,
            ..
        } = event
        else {
            panic!("legacy confirmation")
        };
        assert_eq!(params.reviewers.as_ref().unwrap().len(), 3);
        assert_eq!(
            params
                .preflight
                .as_ref()
                .unwrap()
                .existing_run_id
                .as_deref(),
            Some(original.run_id.as_str())
        );
        assert!(params.preflight.as_ref().unwrap().reviewers.is_empty());
        let disclosure = codex_app_server_client::audit::start_disclosure(&params);
        assert!(
            disclosure.contains("Historical reviewer configuration bindings were not recorded")
        );
        assert!(disclosure.contains("it commissions no new work"));
        assert!(disclosure.contains("Original guidance"));
        disclosures.push(disclosure);
        if let Some(value) = decision {
            answer.send(value).unwrap();
        } else {
            drop(answer);
        }
        let result = task.await?;
        if decision == Some(true) {
            let replay: AuditRunView =
                serde_json::from_value(result.map_err(color_eyre::eyre::Report::msg)?)?;
            assert_eq!(replay.run_id, original.run_id);
            assert_eq!(replay.report, original.report);
        } else {
            assert!(result.is_err());
        }
    }
    let mut changed = args();
    changed.brief = "Changed brief".into();
    let (events, _) = sender();
    assert!(
        execute(
            session.request_handle(),
            thread,
            AuditAction::Start(changed),
            events
        )
        .await
        .is_err()
    );
    let audits = AuditClient::new(session.request_handle());
    assert_eq!(
        audits.attempts(original.run_id.clone()).await?.attempts,
        attempts
    );
    assert_eq!(
        audits.read(original.run_id.clone()).await?.report,
        original.report
    );
    assert_eq!(native.received_requests().await.unwrap().len(), 3);
    assert_eq!(
        fixture::fingerprint(home).map_err(|e| color_eyre::eyre::eyre!("{e:#}"))?,
        before
    );
    if let Some(root) = std::env::var_os("AUDEX_PROCESS_EVIDENCE_DIR") {
        let root = std::path::PathBuf::from(root);
        std::fs::create_dir_all(&root)?;
        std::fs::write(
            root.join("legacy-tui-replay.json"),
            serde_json::to_vec_pretty(
                &json!({"disclosures":disclosures,"attempts":attempts,"run":original,"providerRequests":3,"unchangedStoreSha256":before}),
            )?,
        )?;
    }
    session.shutdown().await?;
    daemon
        .lifecycle("stop")
        .map_err(|e| color_eyre::eyre::eyre!("{e:#}"))?;
    Ok(())
}
