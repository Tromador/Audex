//! Actual result-validator diagnostics over daemon IPC into the TUI history cell.
use super::*;
use crate::history_cell::HistoryCell;
use codex_app_server_protocol::{AuditAttemptReadParams, AuditExtractionStatus};
use sha2::{Digest, Sha256};
#[path = "../../../../cli/tests/audit/subscription_fixture.rs"]
mod fixture;

#[test]
fn model_control_characters_in_extraction_diagnostics_cannot_reach_terminal_history()
-> anyhow::Result<()> {
    core_test_support::test_codex::run_test_with_large_stack("diagnostic-controls", || async {
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
    let child = fixture::configure(home, "diagnostic_controls")
        .map_err(|e| color_eyre::eyre::eyre!("{e:#}"))?;
    let native = responses::start_mock_server().await;
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
name="Unused native fixture"
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
    let action = parse_command(&format!(
        "--snippet 'fn main() {{}}\n' --name example.rs --reviewer '{}'",
        fixture::selection()
    ))
    .map_err(color_eyre::eyre::Report::msg)?
    .action(home)
    .await
    .map_err(color_eyre::eyre::Report::msg)?;
    let (events, mut receive) = sender();
    let task = tokio::spawn(execute(session.request_handle(), thread, action, events));
    let AppEvent::AuditConfirm { decision, .. } = receive.recv().await.unwrap() else {
        panic!("operator confirmation");
    };
    decision.send(true).unwrap();
    let started: AuditRunView =
        serde_json::from_value(task.await?.map_err(color_eyre::eyre::Report::msg)?)?;
    let audits = AuditClient::new(session.request_handle());
    let rows = wait_attempts(&audits, &started.run_id, |a| a.len() == 1 && a[0].completed).await?;
    let attempt = &rows[0];
    let extraction = attempt.extraction.as_ref().unwrap();
    assert_eq!(
        extraction.status,
        AuditExtractionStatus::UnstructuredOpinion
    );
    let diagnostic = extraction.validation_error.as_deref().unwrap();
    assert!(diagnostic.contains("unknown field"));
    assert!(
        diagnostic.contains("\x1b]0;MODEL_TITLE\x07\x1b[31m"),
        "actual serde diagnostic: {diagnostic:?}"
    );
    let params = AuditAttemptReadParams {
        run_id: started.run_id.clone(),
        assignment_id: attempt.assignment_id.clone(),
        after_sequence: None,
        limit: None,
    };
    let original = audits.read_attempt(params.clone()).await?;
    let raw = original.raw_result.as_deref().unwrap();
    assert_eq!(raw, r#"{"\u001b]0;MODEL_TITLE\u0007\u001b[31m": true}"#);
    assert_eq!(
        extraction.raw_digest,
        format!("{:x}", Sha256::digest(raw.as_bytes()))
    );

    let (events, mut receive) = sender();
    observe_attempt(
        session.request_handle(),
        thread,
        attempt.clone(),
        events,
        Arc::new(tokio::sync::Notify::new()),
    )
    .await;
    let AppEvent::AuditAttemptObserved {
        attempt: observed,
        error: None,
        ..
    } = receive.recv().await.unwrap()
    else {
        panic!("retained attempt history event");
    };
    // The exact summary -> info-event cell path used by AuditAttemptObserved.
    let summary = attempt_summary(&observed);
    assert!(!summary.contains('\x1b') && !summary.contains('\x07'));
    let cell = crate::history_cell::new_info_event(summary, None);
    let rendered = cell
        .display_lines(120)
        .into_iter()
        .flat_map(|line| line.spans)
        .map(|span| span.content.into_owned())
        .collect::<String>();
    assert!(rendered.contains("unknown field") && rendered.contains("MODEL_TITLE"));
    assert!(
        !rendered
            .chars()
            .any(|c| c.is_control() && !matches!(c, '\n' | '\t'))
    );
    assert_eq!(
        observed
            .extraction
            .as_ref()
            .unwrap()
            .validation_error
            .as_deref(),
        Some(diagnostic)
    );
    let report = published(&audits, &started.run_id).await?;
    assert!(report.report.as_deref().unwrap().contains("MODEL_TITLE"));
    assert!(!report.report.as_deref().unwrap().contains('\x1b'));
    session.shutdown().await?;
    let session = connect(home).await?;
    let audits = AuditClient::new(session.request_handle());
    assert_eq!(
        audits.read_attempt(params).await?.raw_result,
        original.raw_result
    );
    assert_eq!(audits.read(started.run_id).await?.report, report.report);
    assert_eq!(fixture::prompts(&child), 1);
    assert!(native.received_requests().await.unwrap().is_empty());
    if let Some(root) = std::env::var_os("AUDEX_PROCESS_EVIDENCE_DIR") {
        let root = std::path::PathBuf::from(root);
        std::fs::create_dir_all(&root)?;
        std::fs::write(
            root.join("diagnostic-controls.json"),
            serde_json::to_vec_pretty(&json!({
                "attempt":observed, "raw":original.raw_result, "rendered":rendered, "report":report, "prompts":1
            }))?,
        )?;
    }
    session.shutdown().await?;
    daemon
        .lifecycle("stop")
        .map_err(|e| color_eyre::eyre::eyre!("{e:#}"))?;
    Ok(())
}
