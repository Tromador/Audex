//! Explicitly provisioned dummy client/profile used by CLI, TUI and controller qualification.
use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

pub fn selection() -> Value {
    json!({"provider":"anthropic","model":"claude-haiku-5-5",
        "executionAdapter":"claude_client","authenticationRoute":"claude_subscription"})
}

pub fn configure(home: &Path, mode: &str) -> anyhow::Result<PathBuf> {
    let root = home.join("subscription-fixture");
    std::fs::create_dir_all(&root)?;
    let executable = root.join("client");
    std::fs::write(
        &executable,
        include_str!("../../../app-server/src/audex/external/public_fixture.py"),
    )?;
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700))?;
    std::fs::write(
        root.join("fake_policy.py"),
        include_str!("../../../app-server/src/audex/external/fake_policy.py"),
    )?;
    let profile = root.join("subscription-profile");
    std::fs::create_dir(&profile)?;
    std::fs::set_permissions(&profile, std::fs::Permissions::from_mode(0o700))?;
    std::fs::write(
        profile.join("settings.json"),
        json!({
            "disableAllHooks":true,"disableClaudeAiConnectors":true,"syncClaudeAiPlugins":false,
            "syncClaudeAiSkills":false,"autoMemoryEnabled":false,"crossSessionInbound":"refuse",
            "permissions":{"deny":["SendMessage","ListAgents"]}
        })
        .to_string(),
    )?;
    std::fs::write(
        root.join("policy-scenario.json"),
        json!({"managed_settings":{},"setting_source":"User settings"}).to_string(),
    )?;
    set_mode(&root, mode)?;
    // Kept for the runner to remove after verified child/daemon shutdown, including failures.
    let runtime = tempfile::Builder::new()
        .prefix("")
        .rand_bytes(6)
        .tempdir_in(std::env::var("AUDEX_CLIENT_TEST_RUNTIME_ROOT")?)?
        .keep();
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700))?;
    std::fs::create_dir_all(home.join("audex"))?;
    std::fs::write(
        home.join("audex/reviewer-backends.json"),
        json!({"version":1,
        "claudeSubscription":{"executable":executable,"clientVersion":"2.1.314",
            "profile":profile,"runtimeRoot":runtime}})
        .to_string(),
    )?;
    Ok(root)
}

pub fn set_mode(root: &Path, mode: &str) -> anyhow::Result<()> {
    std::fs::write(root.join("scenario.json"), json!({"mode":mode}).to_string())?;
    Ok(())
}

pub fn prompts(root: &Path) -> usize {
    std::fs::read_to_string(root.join("prompts"))
        .unwrap_or_default()
        .lines()
        .count()
}
