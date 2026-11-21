use super::*;
use pretty_assertions::assert_eq;
use std::sync::Arc;

#[test]
fn inbound_fallback_records_only_variant_names() {
    let directory = tempfile::tempdir().expect("temporary log directory");
    let path = directory.path().join("session.jsonl");
    let logger = SessionLogger::new();
    logger.open(path.clone()).expect("open session log");
    for event in [
        AppEvent::CopySelection {
            text: Arc::from("private plan without parentheses"),
            label: "private label".to_string(),
            format: crate::clipboard_copy::CopyFormat::Markdown,
        },
        AppEvent::CopySelection {
            text: Arc::from("private code(with parentheses)"),
            label: "private label".to_string(),
            format: crate::clipboard_copy::CopyFormat::PlainText,
        },
        AppEvent::ConsolidateProposedPlan("private tuple payload".to_string()),
        AppEvent::SettingsSelectionClosed,
    ] {
        log_inbound_app_event_with(&logger, &event);
    }
    let records = std::fs::read_to_string(path)
        .expect("read session log")
        .lines()
        .map(|line| {
            let mut record: serde_json::Value = serde_json::from_str(line).expect("valid log JSON");
            record.as_object_mut().expect("log object").remove("ts");
            record
        })
        .collect::<Vec<_>>();
    assert_eq!(
        records,
        [
            "CopySelection",
            "CopySelection",
            "ConsolidateProposedPlan",
            "SettingsSelectionClosed"
        ]
        .map(|variant| json!({"dir": "to_tui", "kind": "app_event", "variant": variant}))
    );
}

#[test]
fn session_recording_and_path_ignore_codex_overrides() -> anyhow::Result<()> {
    for (audex_set, codex_record, audex_record, enabled) in [
        (false, None, None, false),
        (false, Some("1"), None, false),
        (true, Some("1"), None, false),
        (false, None, Some("1"), true),
        (true, None, Some("1"), true),
        (false, Some("1"), Some("0"), false),
        (true, Some("1"), Some("1"), true),
    ] {
        let root = tempfile::tempdir()?;
        let upstream = root.path().join("upstream-session.jsonl");
        let selected = root.path().join("audex-session.jsonl");
        let mut child = std::process::Command::new(std::env::current_exe()?);
        child
            .args([
                "session_log::tests::session_log_path_child",
                "--exact",
                "--nocapture",
            ])
            .env("AUDEX_SESSION_LOG_TEST_ROOT", root.path())
            .env(
                "AUDEX_SESSION_LOG_TEST_ENABLED",
                if enabled { "1" } else { "0" },
            )
            .env_remove("CODEX_TUI_RECORD_SESSION")
            .env_remove("AUDEX_TUI_RECORD_SESSION")
            .env("CODEX_TUI_SESSION_LOG_PATH", &upstream)
            .env_remove("AUDEX_TUI_SESSION_LOG_PATH");
        if audex_set {
            child.env("AUDEX_TUI_SESSION_LOG_PATH", &selected);
        }
        if let Some(value) = codex_record {
            child.env("CODEX_TUI_RECORD_SESSION", value);
        }
        if let Some(value) = audex_record {
            child.env("AUDEX_TUI_RECORD_SESSION", value);
        }
        let output = child.output()?;
        assert!(
            output.status.success(),
            "{} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!upstream.exists());
        assert_eq!(selected.exists(), audex_set && enabled);
        if !audex_set && enabled {
            assert_eq!(std::fs::read_dir(root.path().join("logs"))?.count(), 1);
        } else {
            assert!(!root.path().join("logs").exists());
        }
    }
    Ok(())
}

#[tokio::test]
async fn session_log_path_child() -> anyhow::Result<()> {
    let Some(root) = std::env::var_os("AUDEX_SESSION_LOG_TEST_ROOT") else {
        return Ok(());
    };
    let root = PathBuf::from(root);
    let mut config =
        Config::load_default_with_cli_overrides_for_codex_home(root.clone(), Vec::new()).await?;
    config.log_dir = root.join("logs");
    maybe_init(&config);
    assert_eq!(
        LOGGER.is_enabled(),
        std::env::var("AUDEX_SESSION_LOG_TEST_ENABLED")? == "1"
    );
    Ok(())
}
