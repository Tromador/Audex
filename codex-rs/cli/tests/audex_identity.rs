//! Compiled product identity and the existing complete-package daemon workflow.
#![cfg(unix)]
#[path = "app_server_daemon/support.rs"]
mod support;
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Command;

fn executable(name: &str) -> Result<PathBuf> {
    Ok(codex_utils_cargo_bin::cargo_bin(name)?)
}

#[test]
fn compiled_identity_default_home_explicit_home_completion_and_update_fences() -> Result<()> {
    let home = tempfile::tempdir()?;
    for command in [
        vec!["--version"],
        vec!["--help"],
        vec!["exec", "--help"],
        vec!["completion", "bash"],
    ] {
        let out = Command::new(executable("audex")?)
            .args(&command)
            .env("HOME", home.path())
            .env_remove("AUDEX_HOME")
            .output()?;
        ensure!(
            out.status.success(),
            "{:?}: {}",
            command,
            String::from_utf8_lossy(&out.stderr)
        );
        ensure!(
            String::from_utf8_lossy(&out.stdout).contains("audex"),
            "Audex identity absent in {:?}",
            command
        );
    }
    ensure!(home.path().join(".audex").is_dir());
    ensure!(!home.path().join(".chatgpt").exists());
    ensure!(!home.path().join(".codex").exists());
    let explicit = home.path().join("explicit");
    std::fs::create_dir(&explicit)?;
    let out = Command::new(executable("audex")?)
        .arg("--version")
        .env("HOME", home.path())
        .env("AUDEX_HOME", &explicit)
        .output()?;
    ensure!(out.status.success());
    for command in [vec!["update"], vec!["app-server", "daemon", "update"]] {
        let out = Command::new(executable("audex")?)
            .args(command)
            .env("HOME", home.path())
            .env("AUDEX_HOME", &explicit)
            .output()?;
        ensure!(!out.status.success());
        ensure!(String::from_utf8_lossy(&out.stderr).contains("local source packages"));
    }
    let renamed = home.path().join("codex-internal-name");
    std::fs::hard_link(executable("audex")?, &renamed)?;
    let out = Command::new(&renamed)
        .arg("--version")
        .env("HOME", home.path())
        .env("AUDEX_HOME", &explicit)
        .output()?;
    ensure!(String::from_utf8_lossy(&out.stdout).starts_with("audex "));
    let compatibility_home = tempfile::tempdir()?;
    let out = Command::new(executable("codex")?)
        .arg("--version")
        .env("HOME", compatibility_home.path())
        .env_remove("AUDEX_HOME")
        .output()?;
    ensure!(out.status.success());
    ensure!(String::from_utf8_lossy(&out.stdout).starts_with("codex-cli "));
    ensure!(!compatibility_home.path().join(".chatgpt").exists());
    Ok(())
}

#[test]
fn audex_home_separates_configuration_and_authentication() -> Result<()> {
    for (audex_set, codex_set) in [(false, false), (true, false), (false, true), (true, true)] {
        let profile = tempfile::tempdir()?;
        let selected = profile
            .path()
            .join(if audex_set { "selected" } else { ".audex" });
        let upstream = profile.path().join(".codex");
        std::fs::create_dir(&selected)?;
        std::fs::create_dir(&upstream)?;
        let poison = b"model = [invalid config";
        std::fs::write(upstream.join("config.toml"), poison)?;
        std::fs::write(
            selected.join("config.toml"),
            format!(
                "cli_auth_credentials_store = 'file'\nforced_login_method = 'api'\n[projects.'{}']\ntrust_level = 'trusted'\n",
                profile.path().display()
            ),
        )?;
        std::fs::write(
            selected.join("auth.json"),
            r#"{"OPENAI_API_KEY":"sk-home-isolation-test"}"#,
        )?;
        // Home selection must remain fixed while arg0 loads the selected .env.
        std::fs::write(
            selected.join(".env"),
            format!("AUDEX_HOME={}\n", upstream.display()),
        )?;
        let command = || -> Result<Command> {
            let mut cmd = Command::new(executable("audex")?);
            cmd.current_dir(profile.path())
                .env("HOME", profile.path())
                .env_remove("AUDEX_HOME")
                .env_remove("CODEX_HOME")
                .env_remove("AUDEX_SQLITE_HOME")
                .env("CODEX_SQLITE_HOME", &upstream)
                .env_remove("CODEX_ACCESS_TOKEN")
                .env_remove("CODEX_API_KEY")
                .env_remove("OPENAI_API_KEY")
                .env_remove("OPENAI_FEDERATION_RULE_ID")
                .env_remove("OPENAI_IDENTITY_TOKEN_FILE")
                .env_remove("OPENAI_WORKLOAD_IDENTITY_CONTEXT");
            if audex_set {
                cmd.env("AUDEX_HOME", &selected);
            }
            if codex_set {
                cmd.env("CODEX_HOME", &upstream);
            }
            Ok(cmd)
        };
        let out = command()?
            .args(["app-server", "--print-startup-config"])
            .output()?;
        ensure!(
            out.status.success(),
            "{} {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let config: Value = serde_json::from_slice(&out.stdout)?;
        ensure!(
            config["configRequirements"]["requirements"]["allowedLoginMethods"] == json!(["api"])
        );
        let out = command()?.args(["login", "status"]).output()?;
        ensure!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        ensure!(String::from_utf8_lossy(&out.stderr).contains("API key configured"));
        ensure!(std::fs::read(upstream.join("config.toml"))? == poison);
        ensure!(
            std::fs::read_dir(&upstream)?.count() == 1,
            "CODEX_HOME was modified"
        );
        ensure!(!profile.path().join(".chatgpt").exists());
    }
    Ok(())
}

#[test]
fn invalid_audex_home_does_not_fall_back_to_codex_home() -> Result<()> {
    let profile = tempfile::tempdir()?;
    let file = profile.path().join("file");
    std::fs::write(&file, "not a directory")?;
    for (invalid, reason) in [
        (profile.path().join("missing"), "does not exist"),
        (file, "not a directory"),
    ] {
        let output = Command::new(executable("audex")?)
            .args(["app-server", "--print-startup-config"])
            .env("HOME", profile.path())
            .env("AUDEX_HOME", invalid)
            .env("CODEX_HOME", profile.path())
            .output()?;
        ensure!(!output.status.success());
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        ensure!(
            text.contains("AUDEX_HOME") && text.contains(reason),
            "{text}"
        );
        ensure!(!text.contains("CODEX_HOME"), "{text}");
    }
    Ok(())
}

fn link_package(source: &Path, destination: &Path) -> Result<()> {
    std::fs::create_dir(destination)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let to = destination.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            link_package(&entry.path(), &to)?;
        } else if entry.file_name() == "codex-package.json" {
            std::fs::copy(entry.path(), to)?;
        } else {
            ensure!(
                entry.file_type()?.is_file(),
                "fixture package must contain regular inputs"
            );
            std::fs::hard_link(entry.path(), to)?;
        }
    }
    Ok(())
}

#[test]
#[ignore = "requires an explicitly assembled complete local AUDEX_S1_PACKAGE"]
fn complete_audex_package_installs_replaces_stays_pinned_and_launches_tui() -> Result<()> {
    let source = PathBuf::from(std::env::var_os("AUDEX_S1_PACKAGE").context("AUDEX_S1_PACKAGE")?);
    let mut daemon = support::TestDaemon::new()?;
    let package = daemon.home.path().join("audex-package");
    link_package(&source, &package)?;
    daemon.codex = package.join("bin/audex");
    std::fs::write(
        daemon.home.path().join("app-server-daemon/settings.json"),
        r#"{"shutdownGraceSeconds":0,"updater":{"autoUpdateEnabled":false}}"#,
    )?;
    std::fs::write(
        daemon.home.path().join("config.toml"),
        r#"
model="fixture"
model_provider="fixture"
check_for_update_on_startup=false
[model_providers.fixture]
name="Isolated fixture"
base_url="http://127.0.0.1:1/v1"
wire_api="responses"
[features]
plugins=false
remote_plugin=false
plugin_discovery_prefetch=false
"#,
    )?;
    daemon.lifecycle("start")?;
    let mut selections = Vec::new();
    for version in ["0.0.0-s1-first", "0.0.0-s1-second"] {
        let metadata_path = package.join("codex-package.json");
        let mut metadata: Value = serde_json::from_slice(&std::fs::read(&metadata_path)?)?;
        metadata["version"] = version.into();
        std::fs::write(metadata_path, serde_json::to_vec_pretty(&metadata)?)?;
        let out = daemon
            .command()
            .args(["app-server", "daemon", "update", "--from-cli", "--yes"])
            .output()?;
        ensure!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let update: Value = serde_json::from_slice(&out.stdout)?;
        ensure!(
            update["message"]
                .as_str()
                .unwrap_or_default()
                .contains("Audex")
        );
        ensure!(
            !update["message"]
                .as_str()
                .unwrap_or_default()
                .contains("production updates")
        );
        let selected = daemon
            .home
            .path()
            .join("packages/app-server-daemon/current")
            .canonicalize()?;
        let retained: Value =
            serde_json::from_slice(&std::fs::read(selected.join("codex-package.json"))?)?;
        ensure!(retained["variant"] == "audex" && retained["entrypoint"] == "bin/codex");
        ensure!(
            !daemon
                .home
                .path()
                .join("packages/app-server-daemon/auto-update-version")
                .exists()
        );
        let identity = Command::new(selected.join("bin/codex"))
            .arg("--version")
            .env("AUDEX_HOME", daemon.home.path())
            .output()?;
        ensure!(String::from_utf8_lossy(&identity.stdout).starts_with("audex "));
        let audit = daemon.command().args(["audit", "list"]).output()?;
        ensure!(
            audit.status.success(),
            "{}",
            String::from_utf8_lossy(&audit.stderr)
        );
        selections.push(selected);
    }
    ensure!(
        selections[0] != selections[1],
        "replacement generation did not change"
    );
    let fresh_shell = Command::new("/bin/bash")
        .args([
            "--noprofile",
            "--norc",
            "-c",
            "type -P audex; audex --version",
        ])
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin", package.join("bin").display()),
        )
        .env("AUDEX_HOME", daemon.home.path())
        .output()?;
    ensure!(fresh_shell.status.success());
    ensure!(
        String::from_utf8_lossy(&fresh_shell.stdout)
            .contains(package.join("bin/audex").to_str().context("path")?)
    );
    let script = daemon.home.path().join("tui-startup.py");
    std::fs::write(
        &script,
        r#"import fcntl,json,os,pty,select,signal,struct,subprocess,sys,termios,time
master, slave = pty.openpty()
fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH',40,120,0,0))
p = subprocess.Popen([sys.argv[1],'--remote','unix://','--no-alt-screen'],stdin=slave,stdout=slave,stderr=slave,env={**os.environ,'TERM':'xterm-256color'},start_new_session=True)
os.close(slave)
received = b''
try:
    deadline=time.monotonic()+12
    while time.monotonic()<deadline and p.poll() is None:
        if select.select([master],[],[],.1)[0]:
            try: received += os.read(master,65536)
            except OSError: break
        if len(received)>400: break
    assert len(received)>100, 'no TUI output'
    assert any(marker in received for marker in [b'Codex',b'Audex',b'trust',b'Welcome']), received.decode(errors='replace')
    assert p.poll() is None, received.decode(errors='replace')
    os.write(master,b'\x03'); time.sleep(.2); os.write(master,b'\x03')
    try: p.wait(timeout=5)
    except subprocess.TimeoutExpired: os.killpg(p.pid,signal.SIGKILL);p.wait()
    print(json.dumps({'tui_rendered':True,'received_bytes':len(received),'startup_text':received.decode(errors='replace')[-2000:],'model_prompt_sent':False}))
finally:
    if p.poll() is None: os.killpg(p.pid,signal.SIGKILL);p.wait()
    os.close(master)
"#,
    )?;
    let tui = Command::new("python3")
        .arg(script)
        .arg(&daemon.codex)
        .env("AUDEX_HOME", daemon.home.path())
        .output()?;
    ensure!(
        tui.status.success(),
        "{}",
        String::from_utf8_lossy(&tui.stderr)
    );
    let evidence: Value = serde_json::from_slice(&tui.stdout)?;
    ensure!(evidence["tui_rendered"] == true);
    println!(
        "{}",
        json!({"package_generations":selections,"tui":evidence,"operator_environment_changed":false})
    );
    Ok(())
}
