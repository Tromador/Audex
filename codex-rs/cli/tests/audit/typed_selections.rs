use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn typed_cli_rosters_reject_unavailable_members_and_disclose_native_routes() -> Result<()> {
    let server = responses::start_mock_server().await;
    let daemon = configured(&server)?;
    daemon.lifecycle("start")?;
    let source = daemon.home.path().join("example.rs");
    std::fs::write(&source, "fn main() {}\n")?;
    for (executor, route) in [
        ("claude_client", "claude_subscription"),
        ("claude_client", "claude_api"),
        ("native", "ollama_local"),
    ] {
        for position in 0..3 {
            let mut command = audit(&daemon);
            command.arg("--file").arg(&source).arg("--detach");
            for index in 0..3 {
                let selection = if index == position {
                    json!({"provider":"audit_fixture", "model":"audit-test", "executionAdapter":executor, "authenticationRoute":route}).to_string()
                } else {
                    "audit_fixture:audit-test".to_owned()
                };
                command.args(["--reviewer", &selection]);
            }
            let output = command.output()?;
            check(&output, 1)?;
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(stderr.contains(&format!("{}. audit-test / audit_fixture; executor={executor}; authentication route={route}", position + 1)), "{stderr}");
            assert!(stderr.contains("unavailable for commissioning"), "{stderr}");
            assert!(stderr.contains("have not been verified"), "{stderr}");
            let list = audit(&daemon).arg("list").output()?;
            check(&list, 0)?;
            assert!(list.stdout.is_empty(), "no audit rows after rejection");
            assert!(server.received_requests().await.unwrap().is_empty());
        }
    }
    provider(&server, "good").await;
    let native = json!({"provider":"audit_fixture", "model":"audit-test", "executionAdapter":"native", "authenticationRoute":"daemon_configured_native"}).to_string();
    let output = audit(&daemon)
        .arg("--file")
        .arg(&source)
        .args([
            "--reviewer",
            &native,
            "--reviewer",
            "audit_fixture:audit-test",
            "--sequential",
        ])
        .output()?;
    check(&output, 0)?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    for ordinal in 1..=2 {
        assert!(stderr.contains(&format!("{ordinal}. audit-test / audit_fixture; executor=native; authentication route=daemon_configured_native")), "{stderr}");
    }
    assert!(stderr.contains("including queued reviewers"));
    assert_eq!(server.received_requests().await.unwrap().len(), 4);
    Ok(())
}
