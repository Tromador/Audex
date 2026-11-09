//! Read-only policy preflight for the configured Linux client. Never opens OAuth storage.
//! Sources: https://code.claude.com/docs/en/managed-settings and
//! https://code.claude.com/docs/en/server-managed-settings, checked against Linux
//! 2.1.314 (SHA-256 4487042b4c4f5d88d4568f68ae98aae0b1ffae58864cf57fb868641bc5f84594).
//! This is a conservative supported subset, not a second Claude settings engine.
//! The same-process handshake remains the authority for effective settings.
use super::super::snapshot::manifest::sha256_hex;
use serde_json::{Value, json};
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

mod windows;

const MAX_FILE: u64 = 256 * 1024;
const MAX_DROP_INS: usize = 64;
pub(super) const REQUIRED: &[(&str, Value)] = &[
    ("disableAllHooks", Value::Bool(true)),
    ("disableClaudeAiConnectors", Value::Bool(true)),
    ("syncClaudeAiPlugins", Value::Bool(false)),
    ("syncClaudeAiSkills", Value::Bool(false)),
    ("autoMemoryEnabled", Value::Bool(false)),
];

#[derive(Clone)]
pub(super) struct Policy {
    pub profile: PathBuf,
    pub digest: String,
    pub windows_registry: bool,
    pub windows_user_registry: bool,
    pub has_managed: bool,
    machine: PathBuf,
    home: PathBuf,
    credential_binding: Option<Value>,
}

impl Policy {
    pub fn inspect(profile: &Path, home: &Path, machine: &Path) -> anyhow::Result<Self> {
        anyhow::ensure!(
            profile.is_absolute() && profile == profile.canonicalize()? && profile != home,
            "subscription profile must be a resolved dedicated directory"
        );
        let primary = home.join(".claude");
        anyhow::ensure!(
            profile != primary && primary.canonicalize().ok().as_deref() != Some(profile),
            "ordinary Claude profile is not an audit profile"
        );
        let metadata = std::fs::symlink_metadata(profile)?;
        anyhow::ensure!(
            metadata.is_dir()
                && metadata.uid() == unsafe { libc::geteuid() }
                && metadata.mode() & 0o077 == 0,
            "subscription profile must be owned and private"
        );
        let mut evidence = Vec::new();
        // Check isolation now, but allow the official client's normal refresh
        // before get_usage returns. Credential metadata is bound separately then;
        // the policy digest continues to bind configuration from preparation.
        credential_metadata(profile)?;
        let kernel = std::fs::read_to_string("/proc/version")?;
        let registry = PathBuf::from("/mnt/c/Windows/System32/reg.exe");
        let windows = PathBuf::from("/mnt/c/Program Files/ClaudeCode");
        // Deterministic tests supply an entire machine-policy fixture, including
        // the Windows authorities. There is no production override.
        #[cfg(test)]
        let (kernel, registry, windows) = if machine != Path::new("/etc/claude-code") {
            (
                "WSL fixture".into(),
                machine.join("windows-registry"),
                machine.join("windows-policy"),
            )
        } else {
            (kernel, registry, windows)
        };
        let windows = windows::inspect(&kernel, &registry, &windows, &mut evidence)?;
        let user = read_json(&profile.join("settings.json"), &mut evidence)?
            .ok_or_else(|| anyhow::anyhow!("subscription settings are missing"))?;
        validate_effective(&user)?;
        // Client bookkeeping in this file changes during startup. Recheck the
        // route prohibition, but do not bind unrelated mutable client state.
        if let Some(global) = read_json(&profile.join(".claude.json"), &mut Vec::new())? {
            // This is the client's non-secret global configuration, not OAuth storage.
            // An existing Console login must not be selected as a fallback.
            anyhow::ensure!(
                global.get("primaryApiKey").is_none(),
                "profile contains an API login"
            );
        }
        // Cache absence/presence is not eligibility or freshness evidence. Only the
        // same-process personal-tier handshake plus the qualified launch establishes
        // that neither remote loader applies. Still refuse unsafe cached contributions.
        if let Some(cache) = read_json(&profile.join("remote-settings.json"), &mut Vec::new())? {
            validate_document(&cache)?;
        }
        anyhow::ensure!(
            metadata_if_present(&machine.join("managed-mcp.json"))?.is_none(),
            "managed MCP configuration replaces the source-only inventory"
        );
        let mut managed = match windows.settings {
            Some(settings) => settings,
            None => read_linux_policy(machine, &mut evidence)?,
        };
        // Without an admin policy, the pinned WSL loader can select HKCU after
        // double opt-in. The former mandatory admin lock made this tier ineligible.
        let mut windows_user_registry = false;
        if windows.inherits && !windows::has_policy(&managed) {
            if let Some(settings) = windows::user_policy(&registry, &mut evidence)? {
                managed = settings;
                windows_user_registry = windows::has_policy(&managed);
            }
        }
        // Do not emulate cross-tier exceptions (for example a user false wins
        // for plugin/skill synchronisation). Refuse any local managed relaxation
        // even where the pinned client would suppress it. Only the file/drop-in
        // merge above is modelled here; runtime effectiveness comes from the client.
        for (key, expected) in REQUIRED {
            anyhow::ensure!(
                managed.get(*key).is_none_or(|value| value == expected),
                "managed policy relaxes subscription isolation"
            );
        }
        Ok(Self {
            profile: profile.to_owned(),
            windows_registry: windows.registry,
            windows_user_registry,
            has_managed: windows::has_policy(&managed),
            digest: sha256_hex(&serde_json::to_vec(&evidence)?),
            machine: machine.to_owned(),
            home: home.to_owned(),
            credential_binding: None,
        })
    }

    pub fn bind_credentials(&mut self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.credential_binding.is_none(),
            "credentials already bound"
        );
        self.credential_binding = Some(credential_metadata(&self.profile)?);
        Ok(())
    }

    pub fn verify_before_prompt(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.credential_binding.is_some(),
            "credentials not bound after usage"
        );
        self.recheck()
    }

    pub fn recheck(&self) -> anyhow::Result<()> {
        let current = Self::inspect(&self.profile, &self.home, &self.machine)?;
        anyhow::ensure!(
            current.digest == self.digest,
            "subscription policy changed before prompt"
        );
        if let Some(expected) = &self.credential_binding {
            anyhow::ensure!(
                credential_metadata(&self.profile)? == *expected,
                "subscription credentials changed after usage before prompt"
            );
        }
        Ok(())
    }
}

// Only filesystem metadata is inspected; no credential contents are opened.
fn credential_metadata(profile: &Path) -> anyhow::Result<Value> {
    let Some(metadata) = metadata_if_present(&profile.join(".credentials.json"))? else {
        return Ok(Value::Null);
    };
    anyhow::ensure!(
        metadata.is_file()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.nlink() == 1
            && metadata.mode() & 0o077 == 0,
        "subscription credential storage is not isolated"
    );
    Ok(json!({
        "device":metadata.dev(),"inode":metadata.ino(),"size":metadata.len(),
        "mtime":metadata.mtime(),"mtimeNsec":metadata.mtime_nsec(),
        "ctime":metadata.ctime(),"ctimeNsec":metadata.ctime_nsec()
    }))
}

fn read_linux_policy(machine: &Path, evidence: &mut Vec<Value>) -> anyhow::Result<Value> {
    if metadata_if_present(machine)?.is_none() {
        evidence.push(json!({"directory":machine,"absent":true}));
        return Ok(json!({}));
    }
    validate_policy_directory(machine)?;
    let mut managed =
        read_json(&machine.join("managed-settings.json"), evidence)?.unwrap_or_else(|| json!({}));
    validate_document(&managed)?;
    let drop_ins = machine.join("managed-settings.d");
    if metadata_if_present(&drop_ins)?.is_some() {
        validate_policy_directory(&drop_ins)?;
        let mut paths = Vec::new();
        for entry in std::fs::read_dir(&drop_ins)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("invalid policy filename"))?;
            if !name.starts_with('.') && name.ends_with(".json") {
                anyhow::ensure!(name.is_ascii(), "unqualified drop-in ordering");
                paths.push(entry.path());
                anyhow::ensure!(paths.len() <= MAX_DROP_INS, "too many managed drop-ins");
            }
        }
        paths.sort();
        for path in paths {
            let value = read_json(&path, evidence)?
                .ok_or_else(|| anyhow::anyhow!("managed drop-in disappeared"))?;
            validate_document(&value)?;
            merge(&mut managed, &value);
        }
    }
    Ok(managed)
}

fn validate_policy_directory(path: &Path) -> anyhow::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    anyhow::ensure!(
        metadata.is_dir()
            && path == path.canonicalize()?
            && metadata.mode() & 0o022 == 0
            && (metadata.uid() == 0 || metadata.uid() == unsafe { libc::geteuid() }),
        "managed policy directory is not isolated"
    );
    Ok(())
}

fn metadata_if_present(path: &Path) -> anyhow::Result<Option<std::fs::Metadata>> {
    match std::fs::symlink_metadata(path) {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => anyhow::bail!("policy source cannot be inspected"),
    }
}

fn read_json(path: &Path, evidence: &mut Vec<Value>) -> anyhow::Result<Option<Value>> {
    let Some(metadata) = metadata_if_present(path)? else {
        evidence.push(json!({"path":path,"absent":true}));
        return Ok(None);
    };
    anyhow::ensure!(
        metadata.is_file()
            && metadata.nlink() == 1
            && metadata.mode() & 0o022 == 0
            && (metadata.uid() == 0 || metadata.uid() == unsafe { libc::geteuid() }),
        "policy must be an unlinked regular file without shared write access"
    );
    anyhow::ensure!(metadata.len() <= MAX_FILE, "policy file exceeds bound");
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let opened = file.metadata()?;
    anyhow::ensure!(
        metadata.dev() == opened.dev() && metadata.ino() == opened.ino(),
        "policy file changed while opening"
    );
    let mut bytes = Vec::new();
    file.take(MAX_FILE + 1).read_to_end(&mut bytes)?;
    anyhow::ensure!(bytes.len() as u64 <= MAX_FILE, "policy file exceeds bound");
    let value: Value =
        serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("policy is not valid JSON"))?;
    anyhow::ensure!(value.is_object(), "policy is not an object");
    evidence.push(
        json!({"path":path,"digest":sha256_hex(&bytes),"device":opened.dev(),"inode":opened.ino()}),
    );
    Ok(Some(value))
}

// Deliberately bounded supported policy vocabulary. Unrecognised settings may
// affect billing, execution or context; preserving them does not mean enabling them.
pub(super) fn validate_document(value: &Value) -> anyhow::Result<()> {
    let object = value
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("policy is not an object"))?;
    for (key, value) in object {
        let valid = match key.as_str() {
            "disableAllHooks"
            | "disableClaudeAiConnectors"
            | "syncClaudeAiPlugins"
            | "syncClaudeAiSkills"
            | "autoMemoryEnabled"
            | "forceRemoteSettingsRefresh" => value.is_boolean(),
            "crossSessionInbound" => value == "refuse",
            "env" | "hooks" | "extraKnownMarketplaces" | "managedMcpServers" => {
                value.as_object().is_some_and(|v| v.is_empty())
            }
            "enabledPlugins" => value
                .as_object()
                .is_some_and(|v| v.values().all(|v| v == false)),
            "permissions" => value.as_object().is_some_and(|v| {
                v.iter().all(|(key, value)| match key.as_str() {
                    "deny" => value.as_array().is_some_and(|v| {
                        v.iter()
                            .all(|v| matches!(v.as_str(), Some("SendMessage" | "ListAgents")))
                    }),
                    "allow" | "ask" | "additionalDirectories" => {
                        value.as_array().is_some_and(|v| v.is_empty())
                    }
                    "defaultMode" => value == "dontAsk",
                    _ => false,
                })
            }),
            "forceLoginMethod" => value == "claudeai",
            "managedSourcesBehavior" | "parentSettingsBehavior" => value == "first-wins",
            // This delivery flag activates inheritance only in Windows admin
            // sources, inspected separately. It cannot select billing or tools.
            "wslInheritsWindowsSettings" => windows::inheritance(Some(value)).is_ok(),
            "allowAllClaudeAiMcps"
            | "allowManagedPermissionRulesOnly"
            | "autoCompactEnabled"
            | "autoContinueAtUsageLimit"
            | "remoteControlAtStartup" => value == false,
            "theme" | "$schema" => value.is_string(),
            _ => false,
        };
        anyhow::ensure!(valid, "unsupported or incompatible client policy");
    }
    Ok(())
}

pub(super) fn validate_effective(value: &Value) -> anyhow::Result<()> {
    validate_document(value)?;
    for (key, expected) in REQUIRED {
        anyhow::ensure!(
            value[*key] == *expected,
            "required subscription isolation setting is not effective"
        );
    }
    anyhow::ensure!(
        value["crossSessionInbound"] == "refuse",
        "inbound messages are not refused"
    );
    let deny = value["permissions"]["deny"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("agent messaging denial is missing"))?;
    anyhow::ensure!(
        deny.contains(&json!("SendMessage")) && deny.contains(&json!("ListAgents")),
        "agent messaging is not denied"
    );
    Ok(())
}

// Only the vocabulary above reaches this merge: ordinary scalar replacement,
// recursive objects, and deduplicated permission arrays. Special merge keys such
// as fallbackModel and managedMcpServers contributions are refused, not guessed.
fn merge(target: &mut Value, next: &Value) {
    match (target, next) {
        (Value::Object(target), Value::Object(next)) => {
            for (key, value) in next {
                merge(target.entry(key.clone()).or_insert(Value::Null), value);
            }
        }
        (Value::Array(target), Value::Array(next)) => {
            for value in next {
                if !target.contains(value) {
                    target.push(value.clone());
                }
            }
        }
        (target, next) => *target = next.clone(),
    }
}

#[cfg(test)]
mod tests;
