//! Common isolated client input and the accepted pre-prompt policy boundary.
use super::super::snapshot::manifest::sha256_hex;
use super::super::store::{admission::AdmissionIdentity, launch::LaunchRecord};
use super::super::{backends::BackendSettings, retained::RetainedInput, snapshot::SnapshotHandle};
use super::owner::SpawnSpec;
use codex_app_server_protocol::{AuditAuthenticationRoute, AuditExecutionAdapter};
use serde_json::json;
use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::Path;
use std::process::Command;

pub(super) const CLIENT_VERSION: &str = "2.1.314";

pub(super) struct Prepared {
    pub spec: SpawnSpec,
    pub record: LaunchRecord,
    pub prompt: Vec<u8>,
    pub mcp_digest: String,
    pub policy: Option<super::policy::Policy>,
    pub billing_source: &'static str,
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
    let executable = super::commissioning::executable(executable, version)?;
    if route == "claude_subscription" {
        super::commissioning::revalidate(root, settings, policy)?;
    }
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
    let runtime_root = Path::new(runtime_root);
    let host_home = super::commissioning::runtime(runtime_root)?;
    let subscription_policy = if route == "claude_subscription" {
        let profile = Path::new(
            &settings
                .claude_subscription
                .as_ref()
                .expect("selected route")
                .profile,
        );
        Some(super::commissioning::policy(
            root,
            profile,
            runtime_root,
            &host_home,
        )?)
    } else {
        for name in [
            "managed-settings.json",
            "managed-mcp.json",
            "managed-settings.d",
        ] {
            anyhow::ensure!(
                !Path::new("/etc/claude-code").join(name).try_exists()?,
                "API managed policy requires qualification"
            );
        }
        None
    };
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
        subscription_policy,
    )
    .await
    .and_then(|prepared| {
        let retained: serde_json::Value = serde_json::from_str(policy)?;
        if route == "claude_subscription" && retained.get("subscription").is_some() {
            anyhow::ensure!(
                prepared.record.executable_digest.as_deref()
                    == retained["subscription"]["executableDigest"].as_str()
                    && prepared.policy.as_ref().map(|p| p.digest.as_str())
                        == retained["subscription"]["policyDigest"].as_str(),
                "subscription binding changed during launch preparation"
            );
        }
        Ok(prepared)
    });
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
    policy: Option<super::policy::Policy>,
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
            policy
                .as_ref()
                .map(|p| p.profile.clone())
                .unwrap_or_else(|| directory.join("profile"))
                .display()
                .to_string(),
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
    } else {
        // Empty CWD alone does not exclude ancestor project/local settings.
        args.push("--setting-sources=user".into());
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
    let billing_source = if route == "claude_subscription" {
        "none"
    } else {
        "ANTHROPIC_API_KEY"
    };
    #[cfg(test)]
    let mut billing_source = billing_source;
    #[cfg(test)]
    if let Ok(endpoint) = std::fs::read_to_string(root.join("c63-fake-api")) {
        // This compiled-test-only fixture has no normal route or credential path.
        let endpoint = url::Url::parse(endpoint.trim())?;
        anyhow::ensure!(
            endpoint.scheme() == "http"
                && endpoint.host_str() == Some("127.0.0.1")
                && endpoint.port().is_some(),
            "fixture API must be loopback"
        );
        anyhow::ensure!(
            std::fs::read_to_string("/proc/net/route")?.lines().count() == 1,
            "official client fixture requires an isolated network namespace"
        );
        environment.insert("ANTHROPIC_BASE_URL".into(), endpoint.to_string());
        anyhow::ensure!(
            policy
                .as_ref()
                .is_none_or(|p| !p.profile.join(".credentials.json").exists()),
            "fake API must never receive profile credentials"
        );
        environment.insert("ANTHROPIC_API_KEY".into(), "dummy-c63-offline".into());
        billing_source = "ANTHROPIC_API_KEY";
    }
    let executable_digest = sha256_hex(&std::fs::read(executable)?);
    let configuration = json!({"version":1,"executable":executable,"clientVersion":version,
        "executableDigest":executable_digest, "args":args,"environment":environment,
        "cwd":directory.join("cwd"),"mcp":mcp,"promptDigest":sha256_hex(&prompt),
        "subscriptionPolicyDigest":policy.as_ref().map(|p| &p.digest),"policyHandshakeVersion":3});
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
            binding_argument: serde_json::to_string(identity)?,
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
        policy,
        billing_source,
        mcp_digest: sha256_hex(&serde_json::to_vec(&mcp)?),
    })
}
