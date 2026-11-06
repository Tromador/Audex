//! Main-TUI confirmation and exact controls over the public subscription daemon route.
use super::*;
#[path = "../../../../cli/tests/audit/subscription_fixture.rs"]
mod fixture;

#[test]
fn subscription_confirmation_drift_controls_and_read_only_reconnect() -> anyhow::Result<()> {
    core_test_support::test_codex::run_test_with_large_stack("c73-tui-subscription", || async {
        scenario()
            .await
            .map_err(|error| anyhow::anyhow!("{error:#}"))
    })
}

async fn scenario() -> Result<()> {
    let parent = std::env::var_os("AUDEX_TEST_TMPDIR").expect("explicit fixture parent");
    let daemon = support::TestDaemon::new_in(std::path::Path::new(&parent))
        .map_err(|error| color_eyre::eyre::eyre!("{error:#}"))?;
    let home = daemon.home.path();
    let child = fixture::configure(home, "held").map_err(|e| color_eyre::eyre::eyre!("{e:#}"))?;
    let native = responses::start_mock_server().await;
    Mock::given(method("POST")).and(path("/v1/responses")).respond_with(|request:&wiremock::Request| {
        let body:Value=request.body_json().unwrap();
        let assignment=body["input"].as_array().unwrap().iter().filter_map(|i|i["content"].as_array()).flatten()
            .filter_map(|p|p["text"].as_str()).filter_map(|s|serde_json::from_str::<Value>(s).ok())
            .find(|v|v.get("assignment_id").is_some()).unwrap();
        let result=json!({"schema_version":1,"snapshot_id":assignment["snapshot_id"],
            "assignment_id":assignment["assignment_id"],"summary":"Independent TUI native peer","findings":[]});
        ResponseTemplate::new(200).insert_header("content-type","text/event-stream").insert_header("OpenAI-Model","audit-test")
            .set_body_string(responses::sse(vec![responses::ev_response_created("native"),
                responses::ev_assistant_message("answer",&result.to_string()),responses::ev_completed("native")]))
    }).mount(&native).await;
    std::fs::write(
        home.join("config.toml"),
        format!(
            r#"
model="audit-test"
model_provider="audit_fixture"
[features]
plugins=false
remote_plugin=false
plugin_discovery_prefetch=false
[model_providers.audit_fixture]
name="TUI local fixture"
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
        reviewers: Some(vec![
            serde_json::from_value(json!({"provider":"audit_fixture","model":"audit-test"}))
                .unwrap(),
            serde_json::from_value(fixture::selection()).unwrap(),
            serde_json::from_value(json!({"provider":"audit_fixture","model":"audit-test"}))
                .unwrap(),
        ]),
        scheduling: Default::default(),
        expected_served_model: None,
        request_key: None,
        brief: "Review the sealed source only".into(),
        audex_guidance: Some("Explicit source guidance".into()),
        agents_evidence: Some("Explicit context evidence".into()),
    };
    // A No, dismissal and a changed binding while confirmation is open never start a client.
    let mut disclosures = Vec::new();
    for decision in [Some(false), None, Some(true)] {
        let (events, mut receive) = sender();
        let task = tokio::spawn(execute(
            session.request_handle(),
            thread,
            AuditAction::Start(args()),
            events,
        ));
        let AppEvent::AuditConfirm {
            params,
            decision: answer,
            ..
        } = receive.recv().await.unwrap()
        else {
            panic!("confirmation")
        };
        let disclosure = codex_app_server_client::audit::start_disclosure(&params);
        for expected in [
            "including queued reviewers",
            "subscription quota",
            "Official client executable:",
            "Dedicated subscription profile:",
            "Private runtime root:",
            "per occurrence",
            "best effort",
            "Explicit context evidence",
        ] {
            assert!(
                disclosure.contains(expected),
                "missing {expected}: {disclosure}"
            );
        }
        disclosures.push(disclosure);
        assert_eq!(params.preflight.as_ref().unwrap().reviewers.len(), 3);
        assert!(!child.join("launches").exists());
        if decision == Some(true) {
            use std::io::Write;
            std::fs::OpenOptions::new()
                .append(true)
                .open(child.join("client"))?
                .write_all(b"\n# changed after confirmation\n")?;
        }
        if let Some(value) = decision {
            answer.send(value).unwrap();
        } else {
            drop(answer);
        }
        assert!(task.await?.is_err());
        assert!(native.received_requests().await.unwrap().is_empty());
        assert!(!child.join("launches").exists());
        assert!(!home.join("audex/snapshots").exists());
    }
    let started = start_confirmed(&session, thread, args(), Some(true))
        .await?
        .unwrap();
    let id = started.run_id;
    let audits = AuditClient::new(session.request_handle());
    let rows = wait_attempts(&audits, &id, |a| {
        a.len() == 3 && a[0].completed && a[2].completed && a[1].output_sequence >= 7
    })
    .await?;
    assert_eq!(fixture::prompts(&child), 1);
    session.shutdown().await?;
    let session = connect(home).await?;
    let audits = AuditClient::new(session.request_handle());
    for _ in 0..2 {
        let (events, mut receive) = sender();
        execute(
            session.request_handle(),
            thread,
            AuditAction::Attempts(id.clone()),
            events,
        )
        .await
        .map_err(color_eyre::eyre::Report::msg)?;
        let AppEvent::AuditAttemptsLoaded { attempts, .. } = receive.recv().await.unwrap() else {
            panic!("history")
        };
        assert_eq!(
            attempts
                .iter()
                .map(|a| &a.assignment_id)
                .collect::<Vec<_>>(),
            rows.iter().map(|a| &a.assignment_id).collect::<Vec<_>>()
        );
    }
    assert_eq!(fixture::prompts(&child), 1);
    fixture::set_mode(&child, "plain").map_err(|e| color_eyre::eyre::eyre!("{e:#}"))?;
    let retried = control(
        audits.clone(),
        thread,
        rows[1].clone(),
        AuditAttemptAction::Retry,
        "tui-subscription-retry",
    )
    .await?;
    let successor = retried.replacement_assignment_id.unwrap();
    let final_rows = wait_attempts(&audits, &id, |a| {
        a.iter()
            .any(|a| a.assignment_id == successor && a.completed)
    })
    .await?;
    let report = published(&audits, &id).await?;
    assert_eq!(report.phase, AuditRunPhase::Completed);
    let opinion = final_rows
        .iter()
        .find(|a| a.assignment_id == successor)
        .unwrap();
    assert_eq!(
        opinion.extraction.as_ref().unwrap().status,
        codex_app_server_protocol::AuditExtractionStatus::UnstructuredOpinion
    );
    assert!(
        report
            .report
            .as_deref()
            .unwrap()
            .contains("completed opinion; structured findings unavailable")
    );
    assert_eq!(fixture::prompts(&child), 2);
    assert_eq!(native.received_requests().await.unwrap().len(), 2);
    assert_eq!(audits.read(id.clone()).await?.report, report.report);
    if let Some(root) = std::env::var_os("AUDEX_PROCESS_EVIDENCE_DIR") {
        let root = std::path::PathBuf::from(root);
        std::fs::create_dir_all(&root)?;
        std::fs::write(
            root.join("tui-subscription.json"),
            serde_json::to_vec_pretty(
                &json!({"attempts":final_rows,"run":report,"prompts":2,"disclosures":disclosures}),
            )?,
        )?;
    }
    session.shutdown().await?;
    daemon
        .lifecycle("stop")
        .map_err(|e| color_eyre::eyre::eyre!("{e:#}"))?;
    Ok(())
}
