//! Official-client arguments and environment; no policy discovery or source walks.
use super::*;
use crate::audex::backends::{BackendSettings, CredentialReference};
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::process::Command;
use tempfile::TempDir;

pub(super) struct Launch {
    pub command: Command,
    pub prompt: Vec<u8>,
    pub runtime: TempDir,
}

pub(super) fn validate(settings: &BackendSettings, selection: &ReviewerSelection) -> Result<()> {
    ensure!(
        !selection.model.trim().is_empty()
            && !selection.model.starts_with('-')
            && !selection.model.chars().any(char::is_whitespace),
        "Claude model must be a nonempty client model name"
    );
    ensure!(
        selection.effort.is_none(),
        "Claude effort is not configured by this launcher"
    );
    match selection.route.as_str() {
        "claude_subscription" => {
            let config = settings
                .claude_subscription
                .as_ref()
                .context("Claude subscription is not configured")?;
            subscription_profile(Path::new(&config.profile))?;
        }
        "claude_api" => {
            let config = settings
                .claude_api
                .as_ref()
                .context("Claude API is not configured")?;
            credential(&config.credential)?;
        }
        _ => anyhow::bail!("not a Claude reviewer route"),
    }
    Ok(())
}

fn subscription_profile(path: &Path) -> Result<PathBuf> {
    let profile = fs::canonicalize(path).context("Claude subscription profile is unavailable")?;
    let metadata = fs::metadata(&profile)?;
    // SAFETY: geteuid has no arguments or ownership effects.
    ensure!(
        metadata.is_dir()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o077 == 0,
        "Claude subscription profile must be private to the current user"
    );
    let personal = PathBuf::from(std::env::var_os("HOME").context("host HOME is unavailable")?)
        .join(".claude");
    let personal = fs::canonicalize(&personal).unwrap_or(personal);
    ensure!(
        profile != personal,
        "Claude subscription requires a dedicated profile, not ~/.claude"
    );
    Ok(profile)
}

fn credential(reference: &CredentialReference) -> Result<String> {
    let secret = match reference {
        CredentialReference::Environment(name) => std::env::var(name).ok(),
        CredentialReference::File(path) => fs::read_to_string(path).ok(),
    }
    .context("Claude API credential reference is unavailable")?;
    let secret = secret.trim();
    ensure!(
        !secret.is_empty() && !secret.chars().any(char::is_control),
        "Claude API credential is empty or invalid"
    );
    Ok(secret.to_owned())
}

pub(super) fn prepare(
    settings: &BackendSettings,
    selection: &ReviewerSelection,
    directories: &[PathBuf],
    rules: &str,
    assignment: Value,
    audit_run: Option<&Path>,
) -> Result<Launch> {
    validate(settings, selection)?;
    let cwd = directories
        .first()
        .context("Claude needs a working directory")?;
    let (executable, runtime_root) = match selection.route.as_str() {
        "claude_subscription" => {
            let config = settings
                .claude_subscription
                .as_ref()
                .expect("validated route");
            (&config.executable, &config.runtime_root)
        }
        _ => {
            let config = settings.claude_api.as_ref().expect("validated route");
            (&config.executable, &config.runtime_root)
        }
    };
    ensure!(
        Path::new(executable).is_absolute(),
        "Claude executable must be absolute"
    );
    let runtime = tempfile::Builder::new()
        .prefix("audex-claude-")
        .tempdir_in(runtime_root)?;
    for name in ["home", "tmp", "profile"] {
        fs::DirBuilder::new()
            .mode(0o700)
            .create(runtime.path().join(name))?;
    }
    let mut command = Command::new(executable);
    command.args([
        "-p",
        "--restricted",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--model",
        &selection.model,
        "--tools",
        "Read,Grep,Glob",
        "--allowedTools",
        "Read,Grep,Glob",
        "--permission-mode",
        "dontAsk",
        "--strict-mcp-config",
        "--disable-slash-commands",
        "--no-session-persistence",
        "--settings",
        r#"{"permissions":{"blockReadsOutsideWorkingDirectories":true}}"#,
        "--system-prompt",
        rules,
    ]);
    if let Some(run) = audit_run {
        // Follow-up can inspect retained results as well as every live source root.
        command.arg("--add-dir").arg(run);
        for directory in directories {
            command.arg("--add-dir").arg(directory);
        }
    } else {
        command
            .arg("--json-schema")
            .arg(super::super::output_schema().to_string());
        for directory in &directories[1..] {
            command.arg("--add-dir").arg(directory);
        }
    }
    command
        .current_dir(cwd)
        .env_clear()
        .envs([
            ("PATH", "/usr/bin:/bin"),
            ("LANG", "C.UTF-8"),
            ("TERM", "dumb"),
            ("CLAUDE_CODE_DISABLE_CLAUDE_MDS", "1"),
            ("CLAUDE_CODE_DISABLE_AUTO_MEMORY", "1"),
            ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1"),
            ("CLAUDE_CODE_DISABLE_TERMINAL_TITLE", "1"),
        ])
        .env("HOME", runtime.path().join("home"))
        .env("TMPDIR", runtime.path().join("tmp"));
    if selection.route == "claude_subscription" {
        let config = settings
            .claude_subscription
            .as_ref()
            .expect("validated route");
        command.env(
            "CLAUDE_CONFIG_DIR",
            subscription_profile(Path::new(&config.profile))?,
        );
    } else {
        let config = settings.claude_api.as_ref().expect("validated route");
        command
            .arg("--bare")
            .env("CLAUDE_CONFIG_DIR", runtime.path().join("profile"))
            .env("ANTHROPIC_API_KEY", credential(&config.credential)?);
    }
    let mut prompt = serde_json::to_vec(
        &json!({"type":"user","message":{"role":"user","content":assignment.to_string()}}),
    )?;
    prompt.push(b'\n');
    Ok(Launch {
        command,
        prompt,
        runtime,
    })
}
