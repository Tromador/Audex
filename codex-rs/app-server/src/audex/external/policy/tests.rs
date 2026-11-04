use super::*;
mod personal;
use std::os::unix::fs::{PermissionsExt, symlink};

pub(super) struct Fixture {
    _temp: tempfile::TempDir,
    profile: PathBuf,
    home: PathBuf,
    pub(super) machine: PathBuf,
}

impl Fixture {
    pub(super) fn new() -> anyhow::Result<Self> {
        let temp = tempfile::tempdir()?;
        let profile = temp.path().join("profile");
        let home = temp.path().join("home");
        let machine = temp.path().join("system");
        for path in [&profile, &home, &machine] {
            std::fs::create_dir(path)?;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
        }
        std::fs::write(profile.join("settings.json"), settings().to_string())?;
        std::fs::write(
            machine.join("managed-settings.json"),
            r#"{"forceRemoteSettingsRefresh":true}"#,
        )?;
        Ok(Self {
            _temp: temp,
            profile,
            home,
            machine,
        })
    }

    pub(super) fn inspect(&self) -> anyhow::Result<Policy> {
        Policy::inspect(&self.profile, &self.home, &self.machine)
    }
}

fn settings() -> Value {
    json!({"disableAllHooks":true,"disableClaudeAiConnectors":true,"syncClaudeAiPlugins":false,
        "syncClaudeAiSkills":false,"autoMemoryEnabled":false,"crossSessionInbound":"refuse",
        "permissions":{"deny":["SendMessage","ListAgents"]},"theme":"dark"})
}

#[test]
fn dedicated_profile_preserves_settings_and_never_opens_oauth() -> anyhow::Result<()> {
    let f = Fixture::new()?;
    let original = std::fs::read(f.profile.join("settings.json"))?;
    // An unreadable credential is sufficient metadata evidence. No OAuth parser
    // exists in Audex; even invalid bytes must never be opened to qualify policy.
    let credential = f.profile.join(".credentials.json");
    std::fs::write(&credential, b"dummy-not-json")?;
    std::fs::set_permissions(&credential, std::fs::Permissions::from_mode(0))?;
    let mut policy = f.inspect()?;
    policy.bind_credentials()?;
    policy.verify_before_prompt()?;
    assert_eq!(std::fs::read(f.profile.join("settings.json"))?, original);
    std::fs::set_permissions(&credential, std::fs::Permissions::from_mode(0o600))?;
    assert_eq!(std::fs::read(&credential)?, b"dummy-not-json");
    // Changing permissions changes ctime; capture a fresh metadata binding.
    let policy = f.inspect()?;
    // Normal client state creation does not invalidate the non-secret settings binding.
    std::fs::write(
        f.profile.join(".claude.json"),
        r#"{"firstStartVersion":"2.1.314"}"#,
    )?;
    policy.recheck()?;
    std::fs::write(
        f.profile.join(".claude.json"),
        r#"{"primaryApiKey":"dummy"}"#,
    )?;
    assert!(policy.recheck().is_err());
    Ok(())
}

#[test]
fn shared_linked_or_ordinary_profiles_are_refused() -> anyhow::Result<()> {
    let f = Fixture::new()?;
    assert!(Policy::inspect(&f.home, &f.home, &f.machine).is_err());
    symlink(&f.profile, f.home.join(".claude"))?;
    assert!(f.inspect().is_err());
    std::fs::remove_file(f.home.join(".claude"))?;
    std::fs::set_permissions(&f.profile, std::fs::Permissions::from_mode(0o755))?;
    assert!(f.inspect().is_err());
    std::fs::set_permissions(&f.profile, std::fs::Permissions::from_mode(0o700))?;
    symlink(
        f.profile.join("settings.json"),
        f.profile.join(".credentials.json"),
    )?;
    assert!(f.inspect().is_err());
    std::fs::remove_file(f.profile.join(".credentials.json"))?;
    std::fs::hard_link(
        f.profile.join("settings.json"),
        f.profile.join(".credentials.json"),
    )?;
    assert!(f.inspect().is_err());
    Ok(())
}

#[test]
fn managed_file_then_sorted_drop_ins_have_scalar_and_array_precedence() -> anyhow::Result<()> {
    let f = Fixture::new()?;
    let d = f.machine.join("managed-settings.d");
    std::fs::create_dir(&d)?;
    std::fs::write(
        f.machine.join("managed-settings.json"),
        r#"{"forceRemoteSettingsRefresh":true,"autoMemoryEnabled":true}"#,
    )?;
    std::fs::write(
        d.join("20-final.json"),
        r#"{"autoMemoryEnabled":false,"permissions":{"deny":["ListAgents"]}}"#,
    )?;
    std::fs::write(
        d.join("10-earlier.json"),
        r#"{"autoMemoryEnabled":true,"permissions":{"deny":["SendMessage"]}}"#,
    )?;
    // The pinned loader ignores hidden and non-JSON entries.
    std::fs::write(d.join(".hidden.json"), b"invalid")?;
    std::fs::write(d.join("ignored.txt"), b"invalid")?;
    let p = f.inspect()?;
    std::fs::write(d.join("30-unsafe.json"), r#"{"autoMemoryEnabled":true}"#)?;
    assert!(p.recheck().is_err());
    std::fs::remove_file(d.join("30-unsafe.json"))?;
    std::fs::write(d.join("30-safe.json"), r#"{"theme":"light"}"#)?;
    assert!(
        p.recheck().is_err(),
        "safe changes still break the retained binding"
    );
    let mut arrays = json!({"permissions":{"deny":["SendMessage"]}});
    merge(
        &mut arrays,
        &json!({"permissions":{"deny":["SendMessage","ListAgents"]}}),
    );
    assert_eq!(
        arrays["permissions"]["deny"],
        json!(["SendMessage", "ListAgents"])
    );
    Ok(())
}

#[test]
fn unsafe_contributions_are_refused_even_when_shadowed() -> anyhow::Result<()> {
    for value in [
        json!({"env":{"ANTHROPIC_API_KEY":"dummy"}}),
        json!({"env":{"ANTHROPIC_BASE_URL":"https://invalid.example"}}),
        json!({"apiKeyHelper":"dummy"}),
        json!({"awsAuthRefresh":"dummy"}),
        json!({"gcpAuthRefresh":"dummy"}),
        json!({"proxyAuthHelper":"dummy"}),
        json!({"forceLoginMethod":"gateway"}),
        json!({"forceLoginGatewayUrl":"https://invalid.example"}),
        json!({"managedSourcesBehavior":"merge"}),
        json!({"parentSettingsBehavior":"merge"}),
        json!({"policyHelper":{"command":"dummy"}}),
        json!({"hooks":{"SessionStart":[]}}),
        json!({"enabledPlugins":{"dummy":true}}),
        json!({"managedMcpServers":{"dummy":{}}}),
        json!({"fallbackModel":"dummy"}),
        json!({"unknownFuturePolicy":true}),
        json!({"permissions":{"allow":["SendMessage"]}}),
    ] {
        let f = Fixture::new()?;
        let d = f.machine.join("managed-settings.d");
        std::fs::create_dir(&d)?;
        std::fs::write(d.join("10-unsafe.json"), value.to_string())?;
        std::fs::write(d.join("20-safe.json"), settings().to_string())?;
        assert!(f.inspect().is_err(), "unsafe contribution: {value}");
        std::fs::remove_file(d.join("10-unsafe.json"))?;
        std::fs::write(f.profile.join("remote-settings.json"), value.to_string())?;
        assert!(f.inspect().is_err(), "unsafe remote cache: {value}");
    }
    Ok(())
}

#[test]
fn missing_incomplete_and_unsafe_policy_sources_fail_closed() -> anyhow::Result<()> {
    for source in ["managed-settings.json", "managed-settings.d/10-policy.json"] {
        for bytes in [
            b"{".as_slice(),
            b"[]",
            b"{\"forceLoginOrgUUID\":\"dummy-org\"}",
        ] {
            let f = Fixture::new()?;
            let path = f.machine.join(source);
            std::fs::create_dir_all(path.parent().unwrap())?;
            std::fs::write(&path, bytes)?;
            assert!(f.inspect().is_err());
        }
    }
    let f = Fixture::new()?;
    std::fs::remove_file(f.machine.join("managed-settings.json"))?;
    assert!(!f.inspect()?.has_managed);
    let mut user = settings();
    user["forceRemoteSettingsRefresh"] = true.into();
    std::fs::write(f.profile.join("settings.json"), user.to_string())?;
    // A user flag supplies no remote authority; personal-tier qualification is
    // still required in the same process. No admin lock is necessary for it.
    assert!(!f.inspect()?.has_managed);
    std::fs::write(
        f.machine.join("managed-settings.json"),
        r#"{"forceRemoteSettingsRefresh":true}"#,
    )?;
    std::fs::write(f.machine.join("managed-mcp.json"), b"{}")?;
    assert!(f.inspect().is_err());
    std::fs::remove_file(f.machine.join("managed-mcp.json"))?;
    std::fs::set_permissions(&f.machine, std::fs::Permissions::from_mode(0o777))?;
    assert!(f.inspect().is_err());
    std::fs::set_permissions(&f.machine, std::fs::Permissions::from_mode(0o700))?;
    let path = f.profile.join("settings.json");
    std::fs::write(&path, vec![b' '; MAX_FILE as usize + 1])?;
    assert!(f.inspect().is_err());
    std::fs::remove_file(&path)?;
    symlink(f.machine.join("managed-settings.json"), &path)?;
    assert!(f.inspect().is_err());
    Ok(())
}
