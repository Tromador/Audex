use super::*;
use crate::audex::external::policy::tests::Fixture;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::process::ExitStatusExt;

fn registry(f: &Fixture, document: Option<&Value>) -> anyhow::Result<()> {
    let (stdout, stderr, code) = match document {
        Some(value) => (
            format!(
                "HKEY_LOCAL_MACHINE\\SOFTWARE\\Policies\\ClaudeCode\n    Settings    REG_SZ    {value}\n"
            ),
            "",
            0,
        ),
        None => (
            String::new(),
            "ERROR: The system was unable to find the specified registry key or value.\n",
            1,
        ),
    };
    let path = f.machine.join("windows-registry");
    std::fs::write(
        &path,
        format!(
            "#!/usr/bin/python3\nimport os, sys\nassert sys.argv[1:] == ['query', r'HKLM\\SOFTWARE\\Policies\\ClaudeCode', '/v', 'Settings']\nassert not any(k in os.environ for k in ['ANTHROPIC_API_KEY', 'CLAUDE_CODE_OAUTH_TOKEN', 'PATH', 'HOME', 'WSLENV'])\nsys.stdout.write({})\nsys.stderr.write({})\nsys.exit({code})\n",
            serde_json::to_string(&stdout)?,
            serde_json::to_string(stderr)?
        ),
    )?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn file(f: &Fixture, relative: &str, bytes: &[u8]) -> anyhow::Result<()> {
    let path = f.machine.join("windows-policy").join(relative);
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(path, bytes)?;
    Ok(())
}

#[test]
fn ordinary_wsl_with_registry_tool_and_windows_directory_retains_linux() -> anyhow::Result<()> {
    let f = Fixture::new()?;
    registry(&f, None)?;
    let windows = f.machine.join("windows-policy");
    std::fs::create_dir(&windows)?;
    // Typical DrvFS permissions do not prove an active or unreadable policy.
    std::fs::set_permissions(&windows, std::fs::Permissions::from_mode(0o777))?;
    let policy = f.inspect()?;
    assert!(!policy.windows_registry);
    policy.recheck()?;
    // Prove Linux remains authoritative, rather than bypassing its safeguards.
    std::fs::write(
        f.machine.join("managed-settings.json"),
        br#"{"disableAllHooks":false}"#,
    )?;
    assert!(f.inspect().is_err());
    Ok(())
}

#[test]
fn disabled_missing_and_null_flags_do_not_apply_windows_payloads() -> anyhow::Result<()> {
    for flag in [Value::Null, json!(false), json!("false")] {
        let f = Fixture::new()?;
        let value = json!({"wslInheritsWindowsSettings":flag,"env":{"ANTHROPIC_API_KEY":"dummy"}});
        registry(&f, Some(&value))?;
        file(&f, "managed-settings.json", value.to_string().as_bytes())?;
        file(
            &f,
            "managed-settings.d/20-disabled.json",
            value.to_string().as_bytes(),
        )?;
        f.inspect()?.recheck()?;
    }
    let f = Fixture::new()?;
    registry(&f, Some(&json!({"env":{"ANTHROPIC_API_KEY":"dummy"}})))?;
    f.inspect()?;
    Ok(())
}

#[test]
fn flag_only_opt_in_leaves_linux_policy_and_excludes_hkcu() -> anyhow::Result<()> {
    for flag in [json!(true), json!("true")] {
        let f = Fixture::new()?;
        registry(&f, Some(&json!({"wslInheritsWindowsSettings":flag})))?;
        file(
            &f,
            "managed-settings.json",
            b"{\"wslInheritsWindowsSettings\":false}",
        )?;
        file(
            &f,
            "managed-settings.d/20-disabled.json",
            b"{\"wslInheritsWindowsSettings\":false}",
        )?;
        let policy = f.inspect()?;
        assert!(!policy.windows_registry);
        // The registry stub accepts HKLM only. This Linux admin policy makes
        // HKCU ineligible; removing it requires an authoritative HKCU answer.
        policy.recheck()?;
        std::fs::remove_file(f.machine.join("managed-settings.json"))?;
        assert!(
            f.inspect().is_err(),
            "the HKLM-only stub cannot establish eligible HKCU policy"
        );
    }
    Ok(())
}

#[test]
fn enabled_windows_files_use_their_own_policy_and_sorted_drop_ins() -> anyhow::Result<()> {
    let f = Fixture::new()?;
    registry(&f, Some(&json!({"wslInheritsWindowsSettings":false})))?;
    file(
        &f,
        "managed-settings.json",
        br#"{"wslInheritsWindowsSettings":true,"autoMemoryEnabled":true}"#,
    )?;
    assert!(
        f.inspect().is_err(),
        "the selected Windows policy relaxes memory isolation"
    );
    file(
        &f,
        "managed-settings.d/10-lock.json",
        br#"{"forceRemoteSettingsRefresh":true,"autoMemoryEnabled":false}"#,
    )?;
    file(
        &f,
        "managed-settings.d/20-off.json",
        br#"{"wslInheritsWindowsSettings":false}"#,
    )?;
    file(&f, "managed-settings.d/.hidden.json", b"{")?;
    file(&f, "managed-settings.d/ignored.txt", b"{")?;
    std::fs::create_dir(
        f.machine
            .join("windows-policy/managed-settings.d/ignored.json"),
    )?;
    let policy = f.inspect()?;
    assert!(!policy.windows_registry);
    // Removing Linux's lock cannot change an enabled Windows admin selection.
    std::fs::remove_file(f.machine.join("managed-settings.json"))?;
    policy.recheck()?;
    file(
        &f,
        "managed-settings.d/30-relax.json",
        br#"{"autoMemoryEnabled":true}"#,
    )?;
    assert!(f.inspect().is_err());
    Ok(())
}

#[test]
fn any_admin_true_enables_inheritance_and_hklm_precedes_files() -> anyhow::Result<()> {
    let f = Fixture::new()?;
    registry(
        &f,
        Some(&json!({"wslInheritsWindowsSettings":false,"forceRemoteSettingsRefresh":true})),
    )?;
    file(
        &f,
        "managed-settings.json",
        br#"{"wslInheritsWindowsSettings":false}"#,
    )?;
    file(
        &f,
        "managed-settings.d/10-on.json",
        br#"{"wslInheritsWindowsSettings":"true"}"#,
    )?;
    file(
        &f,
        "managed-settings.d/20-off.json",
        br#"{"wslInheritsWindowsSettings":false}"#,
    )?;
    assert!(f.inspect()?.windows_registry);
    // The HKLM document has content, so it wins even though a file armed the chain.
    registry(
        &f,
        Some(&json!({"wslInheritsWindowsSettings":false,"disableAllHooks":false})),
    )?;
    file(
        &f,
        "managed-settings.d/30-lock.json",
        br#"{"forceRemoteSettingsRefresh":true}"#,
    )?;
    assert!(f.inspect().is_err());
    Ok(())
}

#[test]
fn enabled_incompatible_or_unqualified_windows_sources_fail_closed() -> anyhow::Result<()> {
    for unsafe_value in [
        json!({"env":{"ANTHROPIC_API_KEY":"dummy"}}),
        json!({"apiKeyHelper":"dummy"}),
        json!({"hooks":{"SessionStart":[]}}),
        json!({"disableAllHooks":false}),
        json!({"managedSourcesBehavior":"merge"}),
        json!({"unknownPolicy":true}),
    ] {
        for registry_source in [true, false] {
            let f = Fixture::new()?;
            let mut value =
                json!({"wslInheritsWindowsSettings":true,"forceRemoteSettingsRefresh":true});
            merge(&mut value, &unsafe_value);
            if registry_source {
                registry(&f, Some(&value))?;
            } else {
                registry(&f, None)?;
                file(&f, "managed-settings.json", value.to_string().as_bytes())?;
            }
            assert!(f.inspect().is_err());
        }
    }
    let f = Fixture::new()?;
    registry(
        &f,
        Some(&json!({"wslInheritsWindowsSettings":true,"forceRemoteSettingsRefresh":true})),
    )?;
    f.inspect()?;
    file(&f, "managed-mcp.json", b"{}")?;
    assert!(f.inspect().is_err());
    Ok(())
}

#[test]
fn unreadable_malformed_and_indeterminate_admin_sources_block_even_when_disabled()
-> anyhow::Result<()> {
    for bytes in [
        b"{".as_slice(),
        b"[]",
        br#"{"wslInheritsWindowsSettings":"maybe"}"#,
        br#"{"wslInheritsWindowsSettings":1}"#,
    ] {
        for source in ["managed-settings.json", "managed-settings.d/10.json"] {
            let f = Fixture::new()?;
            registry(&f, Some(&json!({"wslInheritsWindowsSettings":false})))?;
            file(&f, source, bytes)?;
            assert!(f.inspect().is_err());
        }
    }
    let f = Fixture::new()?;
    registry(&f, Some(&json!({"wslInheritsWindowsSettings":"maybe"})))?;
    assert!(f.inspect().is_err());
    registry(&f, Some(&json!({"wslInheritsWindowsSettings":false})))?;
    file(&f, "managed-settings.json", b"{}")?;
    let windows = f.machine.join("windows-policy");
    let path = windows.join("managed-settings.json");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0))?;
    let unreadable = f.inspect();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    assert!(unreadable.is_err());
    std::fs::remove_file(&path)?;
    symlink(windows.join("missing"), &path)?;
    assert!(f.inspect().is_err());
    std::fs::remove_file(&path)?;
    file(&f, "managed-settings.d", b"not a directory")?;
    assert!(f.inspect().is_err());
    Ok(())
}

#[test]
fn registry_requires_an_authoritative_answer_and_object() -> anyhow::Result<()> {
    for (code, stdout, stderr) in [
        (1, "", "ERROR: Access is denied."),
        (1, "", "unrecognised diagnostic"),
        (0, "", ""),
        (0, "    Settings    REG_DWORD    1", ""),
        (0, "    Settings    REG_SZ    ", ""),
        (0, "    Settings    REG_SZ    []", ""),
        (0, "    Settings    REG_SZ    {", ""),
        (2, "", MISSING_VALUE),
        (1, "partial value", MISSING_VALUE),
    ] {
        let output = Output {
            status: std::process::ExitStatus::from_raw(code << 8),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        };
        assert!(
            registry_document(&output).is_err(),
            "{code} {stdout} {stderr}"
        );
    }
    for kind in ["REG_SZ", "REG_EXPAND_SZ"] {
        let output = Output {
            status: std::process::ExitStatus::from_raw(0),
            stdout: format!(
                "    Settings    {kind}    {{\n\"wslInheritsWindowsSettings\":false\n}}\r\n"
            )
            .into_bytes(),
            stderr: Vec::new(),
        };
        assert_eq!(
            registry_document(&output)?,
            Some(json!({"wslInheritsWindowsSettings":false}))
        );
    }
    Ok(())
}

#[test]
fn policy_binding_detects_registry_and_activation_changes() -> anyhow::Result<()> {
    let f = Fixture::new()?;
    registry(&f, None)?;
    let policy = f.inspect()?;
    registry(&f, Some(&json!({"wslInheritsWindowsSettings":false})))?;
    assert!(policy.recheck().is_err());
    let policy = f.inspect()?;
    file(
        &f,
        "managed-settings.d/10.json",
        br#"{"wslInheritsWindowsSettings":true}"#,
    )?;
    assert!(policy.recheck().is_err());
    let policy = f.inspect()?;
    file(
        &f,
        "managed-settings.d/10.json",
        br#"{"wslInheritsWindowsSettings":false}"#,
    )?;
    assert!(policy.recheck().is_err());
    Ok(())
}

#[test]
fn non_wsl_never_queries_or_reads_windows_authorities() -> anyhow::Result<()> {
    let f = Fixture::new()?;
    let path = f.machine.join("windows-registry");
    std::fs::write(&path, b"not executable")?;
    file(&f, "managed-settings.json", b"malformed")?;
    let mut evidence = Vec::new();
    let selection = inspect(
        "Linux version 6.8.0-generic",
        &path,
        &f.machine.join("windows-policy"),
        &mut evidence,
    )?;
    assert!(selection.settings.is_none() && !selection.registry);
    assert_eq!(evidence, vec![json!({"wsl":false})]);
    assert!(
        inspect(
            "Microsoft WSL2",
            &path,
            &f.machine.join("windows-policy"),
            &mut Vec::new()
        )
        .is_err()
    );
    Ok(())
}

#[test]
fn registry_probe_is_bounded_and_reaps_after_timeout_or_overflow() -> anyhow::Result<()> {
    for body in [
        "time.sleep(30)",
        "sys.stdout.write('x' * (256 * 1024 + 1)); sys.stdout.flush(); time.sleep(30)",
        "sys.stderr.write('x' * (256 * 1024 + 1)); sys.stderr.flush(); time.sleep(30)",
    ] {
        let f = Fixture::new()?;
        let path = f.machine.join("windows-registry");
        let pid = f.machine.join("probe-pid");
        std::fs::write(
            &path,
            format!(
                "#!/usr/bin/python3\nimport os, sys, time\nopen({}, 'w').write(str(os.getpid()))\n{body}\n",
                serde_json::to_string(&pid)?
            ),
        )?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
        assert!(query(&path, HKLM, Duration::from_millis(300)).is_err());
        let pid: i32 = std::fs::read_to_string(pid)?.parse()?;
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1, "probe must be reaped");
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }
    Ok(())
}
