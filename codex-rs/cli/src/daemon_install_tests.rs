use super::*;

#[test]
fn running_daemon_replacement_prompt() {
    let request = InstallRequest {
        source: "/cli/package".into(),
        version: "0.0.0".into(),
        destination: "/home/packages/app-server-daemon".into(),
        installed_version: Some("0.152.0".into()),
        restart_required: true,
    };
    if crate::application::IS_AUDEX {
        let text = describe_install(&request);
        assert!(text.contains("audex app-server daemon update --from-cli"));
        assert!(!text.contains("return to production updates"));
        assert!(text.contains("active or queued work may be interrupted"));
        return;
    }
    insta::assert_snapshot!(describe_install(&request), @r"
    Replace installed daemon version 0.152.0 with CLI version 0.0.0 from /cli/package.
    The daemon package will be installed in /home/packages/app-server-daemon.
    The selected package will be pinned. Run `codex app-server daemon update` to return to production updates.
    The running daemon will restart; active or queued work may be interrupted.
    ");
}
