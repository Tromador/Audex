//! Retained v1 fixtures shared by controller, public CLI and main-TUI tests.
use std::path::Path;

fn run(mode: &str, home: &Path, args: &[&str]) -> anyhow::Result<String> {
    let output = std::process::Command::new("python3")
        .args(["-c", include_str!("legacy_fixture.py"), mode])
        .arg(home)
        .args(args)
        .output()?;
    anyhow::ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

pub fn retain_v1(home: &Path, run_id: &str) -> anyhow::Result<()> {
    run("v1", home, &[run_id])?;
    Ok(())
}

pub fn fingerprint(home: &Path) -> anyhow::Result<String> {
    run("fingerprint", home, &[])
}
