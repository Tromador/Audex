//! Local subscription commissioning evidence. Inspection never starts the client.
use super::super::backends::BackendSettings;
use super::super::snapshot::manifest::sha256_hex;
use super::policy::Policy;
use codex_app_server_protocol::AuditClaudeBinding;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

pub(super) fn executable(path: &str) -> anyhow::Result<PathBuf> {
    let path = PathBuf::from(path);
    anyhow::ensure!(
        path == path.canonicalize()?
            && path.is_file()
            && std::fs::metadata(&path)?.mode() & 0o111 != 0,
        "client executable must be resolved and executable"
    );
    Ok(path)
}

pub(super) fn runtime(path: &Path) -> anyhow::Result<PathBuf> {
    let home = PathBuf::from(
        std::env::var_os("HOME").ok_or_else(|| anyhow::anyhow!("daemon home unavailable"))?,
    )
    .canonicalize()?;
    anyhow::ensure!(
        path == path.canonicalize()? && !path.starts_with(&home),
        "client runtime must be resolved and outside the daemon home"
    );
    let metadata = std::fs::symlink_metadata(path)?;
    anyhow::ensure!(
        metadata.is_dir()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o077 == 0,
        "runtime root must be an owned private directory"
    );
    let pid_max: u32 = std::fs::read_to_string("/proc/sys/kernel/pid_max")?
        .trim()
        .parse()?;
    anyhow::ensure!(pid_max > 1, "system maximum PID unavailable");
    let socket = path
        .join("00000000-0000-0000-0000-000000000000")
        .join(format!("tmp/cc-socks/{pid_max}.sock"));
    anyhow::ensure!(
        socket.as_os_str().as_bytes().len() <= 103,
        "runtime root exceeds the pinned client's 103-byte socket path limit"
    );
    Ok(home)
}

pub(super) fn policy(
    root: &Path,
    profile: &Path,
    runtime: &Path,
    home: &Path,
) -> anyhow::Result<Policy> {
    anyhow::ensure!(
        !profile.starts_with(runtime) && !runtime.starts_with(profile),
        "profile and runtime must be separate"
    );
    let machine = PathBuf::from("/etc/claude-code");
    #[cfg(test)]
    let machine = if root.join("c71-system-policy").is_dir() {
        root.join("c71-system-policy")
    } else {
        machine
    };
    let _ = root;
    Policy::inspect(profile, home, &machine)
}

pub(crate) fn inspect(
    root: &Path,
    settings: &BackendSettings,
) -> anyhow::Result<AuditClaudeBinding> {
    let config = settings.claude_subscription.as_ref().ok_or_else(|| {
        anyhow::anyhow!(
            "subscription route is unavailable for commissioning: missing client configuration"
        )
    })?;
    let executable = executable(&config.executable)?;
    let runtime_root = Path::new(&config.runtime_root);
    let home = runtime(runtime_root)?;
    let policy = policy(root, Path::new(&config.profile), runtime_root, &home)?;
    Ok(AuditClaudeBinding {
        executable: config.executable.clone(),
        executable_digest: sha256_hex(&std::fs::read(executable)?),
        client_version: config.client_version.clone(),
        profile: config.profile.clone(),
        runtime_root: config.runtime_root.clone(),
        policy_digest: policy.digest,
    })
}

pub(crate) fn retained_policy(binding: &AuditClaudeBinding) -> anyhow::Result<serde_json::Value> {
    Ok(serde_json::json!({
        "version":1, "subscription":binding, "policyHandshakeVersion":3,
        "rules_digest":sha256_hex(super::super::reviewer::AUDIT_RULES.as_bytes()),
        "output_contract_digest":sha256_hex(super::super::reviewer::OUTPUT_CONTRACT.as_bytes()),
        "output_schema_digest":sha256_hex(&serde_json::to_vec(&super::super::result::output_schema()?)?),
    }))
}

pub(crate) fn revalidate(root: &Path, settings: &BackendSettings, raw: &str) -> anyhow::Result<()> {
    let retained: serde_json::Value = serde_json::from_str(raw)?;
    #[cfg(not(test))]
    anyhow::ensure!(
        retained.get("subscription").is_some(),
        "missing subscription qualification binding"
    );
    // Accepted internal qualification fixtures retain their original native policy.
    if retained.get("subscription").is_some() {
        anyhow::ensure!(
            retained == retained_policy(&inspect(root, settings)?)?,
            "subscription executable, profile, runtime or qualified policy changed since approval"
        );
    }
    Ok(())
}
