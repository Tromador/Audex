use super::*;

#[test]
fn personal_preflight_needs_no_machine_policy_but_binds_absence() -> anyhow::Result<()> {
    let f = Fixture::new()?;
    std::fs::remove_file(f.machine.join("managed-settings.json"))?;
    let empty_directory = f.inspect()?;
    assert!(!empty_directory.has_managed);
    empty_directory.recheck()?;
    std::fs::remove_dir(&f.machine)?;
    let absent_directory = f.inspect()?;
    assert!(!absent_directory.has_managed);
    absent_directory.recheck()?;
    std::fs::create_dir(&f.machine)?;
    std::fs::write(
        f.machine.join("managed-settings.json"),
        br#"{"theme":"dark"}"#,
    )?;
    assert!(empty_directory.recheck().is_err());
    assert!(absent_directory.recheck().is_err());
    assert!(f.inspect()?.has_managed);
    Ok(())
}

#[test]
fn refresh_flag_is_optional_and_cache_never_establishes_authority() -> anyhow::Result<()> {
    let f = Fixture::new()?;
    for flag in [true, false] {
        std::fs::write(
            f.machine.join("managed-settings.json"),
            json!({"forceRemoteSettingsRefresh":flag}).to_string(),
        )?;
        f.inspect()?.recheck()?;
    }
    std::fs::remove_file(f.machine.join("managed-settings.json"))?;
    for cache in [json!({}), json!({"forceRemoteSettingsRefresh":true})] {
        std::fs::write(f.profile.join("remote-settings.json"), cache.to_string())?;
        assert!(!f.inspect()?.has_managed);
    }
    std::fs::write(f.profile.join("remote-settings.json"), b"{")?;
    assert!(f.inspect().is_err());
    Ok(())
}

#[test]
fn credentials_bind_after_usage_without_rebinding_policy_or_reading_contents() -> anyhow::Result<()>
{
    let f = Fixture::new()?;
    let path = f.profile.join(".credentials.json");
    let mut policy = f.inspect()?;
    assert!(
        policy.verify_before_prompt().is_err(),
        "unbound cannot submit"
    );
    std::fs::write(&path, b"dummy-original")?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    policy.recheck()?;
    std::fs::write(&path, b"dummy-refreshed")?;
    policy.recheck()?;
    let digest = policy.digest.clone();
    policy.bind_credentials()?;
    assert_eq!(
        policy.digest, digest,
        "binding must preserve policy identity"
    );
    policy.verify_before_prompt()?;
    assert!(
        policy.bind_credentials().is_err(),
        "cannot replace an established binding"
    );
    std::fs::write(&path, b"dummy-replaced")?;
    assert!(policy.verify_before_prompt().is_err());
    let mut replaced = f.inspect()?;
    replaced.bind_credentials()?;
    std::fs::remove_file(&path)?;
    assert!(replaced.verify_before_prompt().is_err());
    let mut absent = f.inspect()?;
    absent.bind_credentials()?;
    absent.verify_before_prompt()?;
    std::fs::write(&path, b"dummy-created")?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    assert!(absent.verify_before_prompt().is_err());
    Ok(())
}

fn registry_pair(f: &Fixture, user: Option<&Value>, failure: bool) -> anyhow::Result<()> {
    let records = json!({
        "HKLM": {"wslInheritsWindowsSettings":true},
        "HKCU": user,
    });
    let script = format!(
        "#!/usr/bin/python3\nimport json, sys\nassert sys.argv[1] == 'query' and sys.argv[3:] == ['/v', 'Settings']\nhive=sys.argv[2].split(chr(92))[0]\nif hive == 'HKCU' and {}:\n print('ERROR: Access is denied.', file=sys.stderr)\n sys.exit(1)\nv=json.loads({})[hive]\nif v is None:\n print('ERROR: The system was unable to find the specified registry key or value.', file=sys.stderr)\n sys.exit(1)\nprint('    Settings    REG_SZ    '+json.dumps(v))\n",
        if failure { "True" } else { "False" },
        serde_json::to_string(&records.to_string())?
    );
    let path = f.machine.join("windows-registry");
    std::fs::write(&path, script)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[test]
fn wsl_hkcu_requires_double_opt_in_and_no_admin_policy() -> anyhow::Result<()> {
    let f = Fixture::new()?;
    std::fs::remove_file(f.machine.join("managed-settings.json"))?;
    for user in [
        None,
        Some(json!({"wslInheritsWindowsSettings":false,"apiKeyHelper":"dummy"})),
    ] {
        registry_pair(&f, user.as_ref(), false)?;
        let policy = f.inspect()?;
        assert!(!policy.has_managed && !policy.windows_user_registry);
        policy.recheck()?;
    }
    let compatible = json!({"wslInheritsWindowsSettings":true,"theme":"dark"});
    registry_pair(&f, Some(&compatible), false)?;
    let policy = f.inspect()?;
    assert!(policy.has_managed && policy.windows_user_registry && !policy.windows_registry);
    policy.recheck()?;
    registry_pair(&f, None, false)?;
    assert!(policy.recheck().is_err());
    for incompatible in [
        json!({"wslInheritsWindowsSettings":true,"disableAllHooks":false}),
        json!({"wslInheritsWindowsSettings":true,"forceLoginOrgUUID":"dummy"}),
        json!({"wslInheritsWindowsSettings":"indeterminate"}),
        json!(["malformed document"]),
    ] {
        registry_pair(&f, Some(&incompatible), false)?;
        assert!(f.inspect().is_err());
    }
    registry_pair(&f, None, true)?;
    assert!(f.inspect().is_err(), "eligible HKCU must be readable");
    std::fs::write(
        f.machine.join("managed-settings.json"),
        br#"{"theme":"dark"}"#,
    )?;
    // A present admin policy makes the unreadable HKCU ineligible again.
    let policy = f.inspect()?;
    assert!(policy.has_managed && !policy.windows_user_registry);
    Ok(())
}
