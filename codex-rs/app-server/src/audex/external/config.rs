//! Common isolated client input. Provider authentication remains unavailable.
use super::super::snapshot::manifest::sha256_hex;
use super::super::store::{admission::AdmissionIdentity, launch::LaunchRecord};
use super::super::{backends::BackendSettings, retained::RetainedInput, snapshot::SnapshotHandle};
use super::owner::SpawnSpec;
use codex_app_server_protocol::{AuditAuthenticationRoute, AuditExecutionAdapter};
use serde_json::json;
use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

pub(super) const CLIENT_VERSION: &str = "2.1.314";

pub(super) struct Prepared {
    pub spec: SpawnSpec,
    pub record: LaunchRecord,
    pub prompt: Vec<u8>,
    pub mcp_digest: String,
}

fn private_directory(path: &Path) -> anyhow::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    anyhow::ensure!(
        metadata.is_dir()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o077 == 0,
        "runtime root must be an owned private directory"
    );
    Ok(())
}

fn create_directory(path: &Path) -> anyhow::Result<()> {
    std::fs::DirBuilder::new().mode(0o700).create(path)?;
    Ok(())
}

pub(super) async fn prepare(
    root: &Path,
    bridge: &Path,
    input: &RetainedInput,
    policy: &str,
    snapshot: &SnapshotHandle,
    identity: &AdmissionIdentity,
    settings: &BackendSettings,
) -> anyhow::Result<Prepared> {
    input.validate_backend_configuration(settings)?;
    anyhow::ensure!(
        input.adapter() == AuditExecutionAdapter::ClaudeClient,
        "wrong external adapter"
    );
    let (executable, version, runtime_root, route) = match input.route() {
        AuditAuthenticationRoute::ClaudeSubscription => {
            let c = settings
                .claude_subscription
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("missing client configuration"))?;
            (
                &c.executable,
                &c.client_version,
                &c.runtime_root,
                "claude_subscription",
            )
        }
        AuditAuthenticationRoute::ClaudeApi => {
            let c = settings
                .claude_api
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("missing client configuration"))?;
            (
                &c.executable,
                &c.client_version,
                &c.runtime_root,
                "claude_api",
            )
        }
        _ => anyhow::bail!("wrong external route"),
    };
    anyhow::ensure!(version == CLIENT_VERSION, "unqualified client version");
    let executable = PathBuf::from(executable);
    anyhow::ensure!(
        executable == executable.canonicalize()? && executable.is_file(),
        "client executable must be resolved"
    );
    let binding = input
        .bindings
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("missing external bindings"))?;
    let tools = super::super::source::TOOL_NAMES
        .map(|name| format!("mcp__audex_source__{name}"))
        .to_vec();
    anyhow::ensure!(
        binding.snapshot_id == snapshot.snapshot_id().to_string()
            && binding.manifest_digest == sha256_hex(&snapshot.manifest().to_pretty_bytes()?)
            && binding.context_digest
                == sha256_hex(&serde_json::to_vec(
                    &snapshot.manifest().identity.context_documents
                )?)
            && binding.policy_digest == sha256_hex(policy.as_bytes())
            && binding.tool_exposure == tools
            && binding.native_configuration_identity.is_none()
            && binding.limits.request_retries == 0
            && binding.limits.stream_retries == 0
            && !binding.limits.automatic_compaction
            && binding.limits.tool_argument_bytes == 16 * 1024
            && binding.limits.read_bytes == 64 * 1024
            && binding.limits.search_query_bytes
                == super::super::snapshot::AccessLimits::default().max_search_query_bytes
            && binding.limits.search_results
                == super::super::snapshot::AccessLimits::default().max_search_results,
        "retained external policy mismatch"
    );
    // System-wide managed client policy cannot be disabled by env_clear. Until
    // route qualification owns its interpretation, reject any such contribution.
    for path in [
        "/etc/claude-code/managed-settings.json",
        "/etc/claude-code/managed-mcp.json",
    ] {
        anyhow::ensure!(
            !Path::new(path).try_exists()?,
            "managed client configuration requires qualification"
        );
    }
    let runtime_root = Path::new(runtime_root);
    let host_home =
        std::env::var_os("HOME").ok_or_else(|| anyhow::anyhow!("daemon home unavailable"))?;
    let host_home = PathBuf::from(host_home).canonicalize()?;
    anyhow::ensure!(
        !runtime_root.starts_with(&host_home),
        "client runtime must be outside the daemon home"
    );
    private_directory(runtime_root)?;
    anyhow::ensure!(
        runtime_root == runtime_root.canonicalize()?,
        "runtime root must be resolved"
    );
    let directory = runtime_root.join(&identity.execution_id);
    create_directory(&directory)?;
    let result = prepare_files(
        root,
        bridge,
        input,
        snapshot,
        identity,
        &directory,
        &executable,
        version,
        route,
        tools,
    )
    .await;
    if result.is_err() {
        // This unique execution directory was created above by this invocation.
        let _ = std::fs::remove_dir_all(&directory);
    }
    result
}

async fn prepare_files(
    root: &Path,
    bridge: &Path,
    input: &RetainedInput,
    snapshot: &SnapshotHandle,
    identity: &AdmissionIdentity,
    directory: &Path,
    executable: &Path,
    version: &str,
    route: &str,
    tools: Vec<String>,
) -> anyhow::Result<Prepared> {
    for name in ["cwd", "home", "profile", "tmp", "bridge-home"] {
        create_directory(&directory.join(name))?;
    }
    let mcp_path = directory.join(format!("mcp-{}.json", identity.execution_id));
    let binding = input.bindings.as_ref().expect("validated bindings");
    let mcp = json!({"mcpServers":{"audex_source":{
        "type":"stdio", "command":bridge, "args":["audit-source-mcp",
            "--state-root",root,"--binding",serde_json::to_string(identity)?,
            "--snapshot",binding.snapshot_id,"--manifest-digest",binding.manifest_digest,
            "--parent-pid","0"],
        "env":{"CODEX_HOME":directory.join("bridge-home")}
    }}});
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&mcp_path)?;
    file.write_all(&serde_json::to_vec(&mcp)?)?;
    file.sync_all()?;
    std::fs::File::open(directory)?.sync_all()?;
    let mut environment: BTreeMap<String, String> = BTreeMap::from([
        ("PATH".into(), "/usr/bin:/bin".into()),
        ("USER".into(), "audex-reviewer".into()),
        ("LANG".into(), "C.UTF-8".into()),
        ("TERM".into(), "dumb".into()),
        ("HOME".into(), directory.join("home").display().to_string()),
        ("TMPDIR".into(), directory.join("tmp").display().to_string()),
        (
            "CLAUDE_CONFIG_DIR".into(),
            directory.join("profile").display().to_string(),
        ),
    ]);
    for (key, value) in [
        ("CLAUDE_CODE_MAX_RETRIES", "0"),
        ("CLAUDE_CODE_DISABLE_NONSTREAMING_FALLBACK", "1"),
        ("CLAUDE_CODE_NONSTREAMING_TIMEOUT_RETRIES", "0"),
        ("CLAUDE_CODE_DISABLE_REFUSAL_FALLBACK", "1"),
        ("CLAUDE_CODE_DISABLE_REFUSAL_RETRY", "1"),
        ("DISABLE_COMPACT", "1"),
        ("CLAUDE_CODE_DISABLE_AUTO_MEMORY", "1"),
        ("CLAUDE_CODE_DISABLE_CLAUDE_MDS", "1"),
        ("CLAUDE_CODE_DISABLE_TERMINAL_TITLE", "1"),
        ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1"),
    ] {
        environment.insert(key.into(), value.into());
    }
    let mut args: Vec<String> = [
        "-p",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--include-partial-messages",
        "--model",
        input.resolved_model.as_deref().expect("decoded model"),
        "--tools",
        "",
        "--strict-mcp-config",
        "--mcp-config",
        mcp_path
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("non UTF-8 runtime"))?,
        "--permission-mode",
        "dontAsk",
        "--allowedTools",
        &tools.join(","),
        "--disable-slash-commands",
        "--no-session-persistence",
        "--system-prompt",
        super::super::reviewer::AUDIT_RULES,
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    if route == "claude_api" {
        args.push("--bare".into());
    }
    let context = snapshot
        .context_documents()
        .await?
        .into_iter()
        .map(|(path, digest, bytes)| {
            Ok(json!({"path":path,"sha256":digest,"untrusted_content":String::from_utf8(bytes)?}))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let assignment = json!({"version":1,"run_id":identity.run_id,"assignment_id":identity.assignment_id,
        "snapshot_id":snapshot.snapshot_id(),"brief":input.brief,"context":context,
        "output_contract":super::super::reviewer::OUTPUT_CONTRACT,"output_schema":super::super::result::output_schema()?});
    let mut prompt = serde_json::to_vec(
        &json!({"type":"user","message":{"role":"user","content":assignment.to_string()}}),
    )?;
    prompt.push(b'\n');
    anyhow::ensure!(prompt.len() <= 256 * 1024, "external prompt exceeds bound");
    let executable_digest = sha256_hex(&std::fs::read(executable)?);
    let configuration = json!({"version":1,"executable":executable,"clientVersion":version,
        "executableDigest":executable_digest, "args":args,"environment":environment,
        "cwd":directory.join("cwd"),"mcp":mcp,"promptDigest":sha256_hex(&prompt)});
    let mut command = Command::new(executable);
    command
        .args(args)
        .env_clear()
        .envs(environment)
        .current_dir(directory.join("cwd"));
    Ok(Prepared {
        spec: SpawnSpec {
            command,
            nonce: identity.execution_id.clone(),
            nonce_argument: mcp_path.display().to_string(),
            executable_digest: executable_digest.clone(),
        },
        record: LaunchRecord {
            operation_id: identity.operation_id.clone(),
            execution_id: identity.execution_id.clone(),
            route: route.into(),
            configuration_identity: binding.backend_configuration_identity.clone(),
            launch_configuration_digest: sha256_hex(&serde_json::to_vec(&configuration)?),
            executable: executable.display().to_string(),
            executable_digest: Some(executable_digest),
            client_session_id: None,
            client_accepted_at_ms: None,
            client_version: version.into(),
            mcp_config_path: mcp_path.display().to_string(),
            intent_at_ms: 0,
            pid: None,
            start_ticks: None,
            boot_id: None,
            process_group: None,
            parent_pid: None,
            identity_at_ms: None,
            prompt_write_started_at_ms: None,
        },
        prompt,
        mcp_digest: sha256_hex(&serde_json::to_vec(&mcp)?),
    })
}
