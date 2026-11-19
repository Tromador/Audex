//! Executes the constructed permission profile through Codex's Linux sandbox.
use super::*;

#[cfg(target_os = "linux")]
#[tokio::test]
async fn effective_profile_confines_actual_commands_without_inference() -> Result<()> {
    use codex_protocol::config_types::WindowsSandboxLevel;
    use codex_protocol::shell_environment::create_env_from_vars;
    use codex_sandboxing::{
        SandboxCommand, SandboxExecutables, SandboxManager, SandboxNetwork, SandboxSelection,
        SandboxTransformRequest, SandboxType, SandboxablePreference,
    };
    use codex_utils_path_uri::PathUri;
    use std::collections::HashMap;
    use std::net::TcpListener;
    use std::process::{Command, Output};

    let temp = tempfile::tempdir_in(std::env::var("HOME")?)?;
    let (host, run, models) = fixture(&temp).await?;
    let canary = temp.path().join("outside-canary");
    fs::write(&canary, "outside canary must remain unreadable\n")?;
    std::os::unix::fs::symlink(&canary, run.targets.working_directories[0].join("escape"))?;
    assert!(
        Path::new("/mnt/c").is_dir(),
        "qualification requires the host's /mnt/c mount"
    );
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let network_probe = format!(
        "import socket; s=socket.socket(); s.settimeout(2); s.connect(('127.0.0.1', {})); print('connected')",
        listener.local_addr()?.port()
    );
    assert!(
        Command::new("/usr/bin/python3")
            .args(["-c", &network_probe])
            .status()?
            .success(),
        "host network positive control"
    );
    assert!(
        Command::new("/bin/cat")
            .arg(&canary)
            .output()?
            .status
            .success(),
        "host canary positive control"
    );
    let prepared = prepare(&host, &models, &run, &selection()).await?;
    let config = &prepared.options.config;
    // Artificial parent only: never enumerate or print the test process environment.
    let runtime = HashMap::from([
        ("PATH".into(), "/usr/bin:/bin".into()),
        ("SHELL".into(), "/bin/sh".into()),
        ("HOME".into(), std::env::var("HOME")?),
        ("TMPDIR".into(), std::env::var("TMPDIR")?),
        ("LANG".into(), "C".into()),
    ]);
    let secrets = [
        ("OPENAI_API_KEY", "synthetic-key-canary"),
        ("AUDIT_SECRET", "synthetic-secret-canary"),
        ("ACCESS_TOKEN", "synthetic-token-canary"),
        ("DATABASE_URL", "synthetic-database-canary"),
        ("SESSION_COOKIE", "synthetic-cookie-canary"),
    ];
    let mut parent = runtime.clone();
    parent.extend(
        secrets
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string())),
    );
    let default_env =
        create_env_from_vars(parent.clone(), &ShellEnvironmentPolicy::default(), None);
    for (key, value) in secrets {
        assert_eq!(default_env.get(key).map(String::as_str), Some(value));
    }
    let shell_env =
        create_env_from_vars(parent, &config.permissions.shell_environment_policy, None);
    assert_eq!(shell_env, runtime);
    println!(
        "Synthetic parent policy check: 5 core variables retained; all 5 fake secrets excluded, including DATABASE_URL and SESSION_COOKIE."
    );
    let profile = config.permissions.effective_permission_profile();
    println!(
        "Effective reviewer profile: {}",
        serde_json::to_string(&profile)?
    );
    let cwd = PathUri::from(config.cwd.clone());
    let sandbox_exe = host.codex_linux_sandbox_exe.as_deref().unwrap();
    let execute = |argv: Vec<String>| -> Result<Output> {
        let command = SandboxManager::new().transform(SandboxTransformRequest {
            command: SandboxCommand {
                program: argv[0].clone().into(),
                args: argv[1..].to_vec(),
                cwd: cwd.clone(),
                env: shell_env.clone(),
                managed_network: None,
                additional_permissions: None,
            },
            permissions: &profile,
            workspace_roots: None,
            selection: SandboxSelection::Platform {
                preference: SandboxablePreference::Require,
                windows_sandbox: SandboxType::None,
            },
            network: SandboxNetwork::default(),
            sandbox_policy_cwd: &cwd,
            executables: SandboxExecutables::Launcher(Some(sandbox_exe)),
            use_legacy_landlock: false,
            windows_sandbox_level: WindowsSandboxLevel::Disabled,
        })?;
        let mut command = command.into_direct_command()?;
        println!("Command: {command:?}");
        let output = command.output()?;
        println!(
            "Exit: {}; stdout: {}; stderr: {}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(output)
    };
    let environment = execute(vec!["/usr/bin/env".into()])?;
    assert!(environment.status.success());
    let environment = String::from_utf8(environment.stdout)?;
    for (key, value) in &runtime {
        assert!(
            environment
                .lines()
                .any(|line| line == format!("{key}={value}"))
        );
    }
    for (key, value) in secrets {
        assert!(!environment.contains(key));
        assert!(!environment.contains(value));
    }
    for argv in [
        vec![
            "/usr/bin/rg".into(),
            "-n".into(),
            "source marker".into(),
            "named.rs".into(),
        ],
        vec!["/bin/cat".into(), "named.rs".into()],
        vec![
            "/bin/cat".into(),
            run.targets.paths[1].display().to_string(),
        ],
    ] {
        assert!(
            execute(argv)?.status.success(),
            "authorised source read failed"
        );
    }
    for argv in [
        vec!["/bin/cat".into(), canary.display().to_string()],
        vec!["/bin/cat".into(), "escape".into()],
        vec!["/bin/ls".into(), "/mnt/c".into()],
        vec![
            "/bin/cat".into(),
            sandbox_exe
                .parent()
                .unwrap()
                .join("codex-app-server")
                .display()
                .to_string(),
        ],
        vec![
            "/bin/sh".into(),
            "-c".into(),
            "printf forbidden > named.rs".into(),
        ],
        vec![
            "/bin/sh".into(),
            "-c".into(),
            "printf forbidden > new-file".into(),
        ],
        vec![
            "/bin/sh".into(),
            "-c".into(),
            "printf forbidden > \"$TMPDIR/audex-s2-write\"".into(),
        ],
        vec!["/usr/bin/python3".into(), "-c".into(), network_probe],
    ] {
        assert!(
            !execute(argv)?.status.success(),
            "sandbox allowed forbidden access"
        );
    }
    assert_eq!(
        fs::read_to_string(&run.targets.paths[0])?,
        "source marker\nsecond line\n"
    );
    assert!(!run.targets.working_directories[0].join("new-file").exists());
    Ok(())
}
