//! Real CLI replay of retained v1 inputs after the provider configuration disappears.
use super::*;
#[path = "legacy_fixture.rs"]
mod fixture;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn legacy_cli_singular_and_roster_replay_without_new_execution() -> Result<()> {
    let provider = NativeProvider::new(false, "match").await?;
    let daemon = configure(&provider, false)?;
    daemon.lifecycle("start")?;
    let path = source(&daemon)?;
    let mut singular = audit(&daemon);
    singular.arg("--file").arg(&path).args([
        "--provider",
        "audit_fixture",
        "--model",
        "audit-test",
        "--request-key",
        "legacy-singular",
    ]);
    let roster = roster_command(
        &daemon,
        &path,
        "legacy-roster",
        &["audit-test", "other-native"],
    );
    let mut cases = Vec::new();
    for mut command in [singular, roster] {
        let output = command.output()?;
        check(&output, 0)?;
        let id = run_id(&output)?;
        cases.push((command, attempts(&daemon, &id)?, id, output.stdout));
    }
    assert_eq!(provider.count(), 6);
    daemon.lifecycle("stop")?;
    for (_, _, id, _) in &cases {
        fixture::retain_v1(daemon.home.path(), id)?;
    }
    let config = daemon.home.path().join("config.toml");
    std::fs::write(
        &config,
        std::fs::read_to_string(&config)?.replace("audit_fixture", "unavailable-original-provider"),
    )?;
    let before = fixture::fingerprint(daemon.home.path())?;
    daemon.lifecycle("start")?;
    for (mut command, records, id, report) in cases {
        let replay = command.arg("--detach").output()?;
        check(&replay, 0)?;
        assert_eq!(String::from_utf8_lossy(&replay.stdout).trim(), id);
        let disclosure = String::from_utf8_lossy(&replay.stderr);
        assert!(
            disclosure.contains("Historical reviewer configuration bindings were not recorded")
        );
        assert!(disclosure.contains("it commissions no new work"));
        assert_eq!(attempts(&daemon, &id)?, records);
        let read = audit(&daemon).args(["read", &id]).output()?;
        check(&read, 0)?;
        assert_eq!(read.stdout, report);
        let conflict = command.args(["--brief", "Changed brief"]).output()?;
        check(&conflict, 1)?;
        let error = String::from_utf8_lossy(&conflict.stderr);
        assert!(error.contains("idempotency key conflicts"), "{error}");
    }
    assert_eq!(fixture::fingerprint(daemon.home.path())?, before);
    assert_eq!(provider.count(), 6);
    daemon.lifecycle("stop")?;
    provider.shutdown().await;
    Ok(())
}
