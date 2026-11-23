//! Official-client arguments and environment; no policy discovery or source walks.
use super::*;
use crate::audex::backends::{BackendSettings, CredentialReference};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
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
    if let Some(effort) = selection.effort.as_deref() {
        ensure!(
            matches!(effort, "low" | "medium" | "high" | "xhigh" | "max"),
            "Claude effort must be low, medium, high, xhigh or max"
        );
    }
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

fn user_mcp_config(include_zen: bool) -> Result<Option<(String, Vec<String>)>> {
    let home = PathBuf::from(std::env::var_os("HOME").context("host HOME is unavailable")?);
    let bytes = match fs::read(home.join(".claude.json")) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let settings: Value =
        serde_json::from_slice(&bytes).context("invalid Claude user configuration")?;
    let Some(servers) = settings.get("mcpServers").and_then(Value::as_object) else {
        return Ok(None);
    };
    select_mcp_servers(servers, include_zen)
}

pub(super) fn select_mcp_servers(
    servers: &serde_json::Map<String, Value>,
    include_zen: bool,
) -> Result<Option<(String, Vec<String>)>> {
    let mut selected = serde_json::Map::new();
    let mut permissions = Vec::new();
    for (name, names) in [
        ("context7", &["resolve-library-id", "query-docs"][..]),
        (
            "github",
            &[
                "compare_commits",
                "fetch",
                "fetch_commit",
                "fetch_file",
                "fetch_pr",
                "fetch_pr_file_patch",
                "get_pr_diff",
                "get_pr_info",
                "get_repo",
                "search",
                "search_branches",
                "search_commits",
                "search_repositories",
            ][..],
        ),
    ] {
        if let Some(server) = servers.get(name) {
            selected.insert(name.to_owned(), server.clone());
            permissions.extend(names.iter().map(|tool| format!("mcp__{name}__{tool}")));
        }
    }
    if include_zen {
        let name = "zen-storage";
        let server = servers
            .get(name)
            .context("Zen Storage is not configured for Claude")?;
        selected.insert(name.into(), server.clone());
        permissions.extend(
            [
                "read_text_file",
                "read_media_file",
                "read_multiple_files",
                "get_file_info",
                "list_directory",
                "list_directory_with_sizes",
                "directory_tree",
                "search_files",
                "list_allowed_directories",
            ]
            .into_iter()
            .map(|tool| format!("mcp__{name}__{tool}")),
        );
    }
    if selected.is_empty() {
        return Ok(None);
    }
    Ok(Some((json!({"mcpServers":selected}).to_string(), permissions)))
}

pub(super) fn prepare(
    settings: &BackendSettings,
    selection: &ReviewerSelection,
    directories: &[PathBuf],
    rules: &str,
    assignment: Value,
    audit_run: Option<&Path>,
    zen_only: bool,
) -> Result<Launch> {
    prepare_inner(
        settings,
        selection,
        directories,
        rules,
        assignment,
        audit_run,
        false,
        zen_only,
    )
}

pub(super) fn prepare_conversation(
    settings: &BackendSettings,
    model: &str,
    rules: &str,
    assignment: Value,
) -> Result<Launch> {
    let selection = ReviewerSelection {
        label: "conversation".into(),
        route: "claude_subscription".into(),
        model: model.into(),
        effort: None,
        permission_summary: String::new(),
    };
    prepare_inner(settings, &selection, &[], rules, assignment, None, true, false)
}

fn prepare_inner(
    settings: &BackendSettings,
    selection: &ReviewerSelection,
    directories: &[PathBuf],
    rules: &str,
    assignment: Value,
    audit_run: Option<&Path>,
    conversation: bool,
    zen_only: bool,
) -> Result<Launch> {
    validate(settings, selection)?;
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
    let conversation_cwd = runtime.path().join("cwd");
    let cwd = if conversation {
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&conversation_cwd)?;
        &conversation_cwd
    } else {
        directories
            .first()
            .context("Claude needs a working directory")?
    };
    let tools = if conversation { "" } else { "Read,Grep,Glob,WebSearch" };
    let mcp = if conversation {
        None
    } else {
        user_mcp_config(zen_only)?
    };
    let mut command = Command::new(executable);
    command.arg("-p");
    if conversation {
        command.arg("--restricted");
    }
    command.args([
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--model",
        &selection.model,
        "--tools",
        tools,
    ]);
    if let Some(effort) = selection.effort.as_deref() {
        command.args(["--effort", effort]);
    }
    if !conversation {
        command.args(["--setting-sources", "user", "--allowedTools", tools]);
        if let Some((config, permissions)) = mcp {
            let path = runtime.path().join("mcp.json");
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)?;
            file.write_all(config.as_bytes())?;
            command.args(permissions);
            command.arg("--mcp-config").arg(path);
        }
    }
    command.args([
        "--permission-mode",
        "dontAsk",
        "--disable-slash-commands",
        "--no-session-persistence",
        "--settings",
        r#"{"disableAllHooks":true,"permissions":{"blockReadsOutsideWorkingDirectories":true}}"#,
        "--system-prompt",
        rules,
    ]);
    command.arg("--strict-mcp-config");
    if conversation {
        command.arg("--include-partial-messages");
    } else if let Some(run) = audit_run {
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
