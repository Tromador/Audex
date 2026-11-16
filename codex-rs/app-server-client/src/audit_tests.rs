use super::*;
use serde_json::json;

#[test]
fn protocol_nine_rejects_older_and_newer_without_fallback() {
    assert_eq!(AUDIT_PROTOCOL_VERSION, 9);
    assert!(require_audit_protocol_version(9).is_ok());
    for version in [0, 5, 6, 7, 8, 10] {
        let error = require_audit_protocol_version(version).unwrap_err();
        assert!(!error.start_acknowledgement_uncertain());
        assert!(error.retained_run_id().is_none());
        assert!(error.to_string().contains("requires 9"));
    }
}

#[test]
fn disclosure_preserves_order_duplicates_and_actual_routes() {
    let mut params: AuditStartParams = serde_json::from_value(json!({
        "requestKey":"key", "source":{"type":"snippet", "name":"x", "content":"x"},
        "brief":"brief", "provider":"default-provider", "model":"default-model"
    }))
    .unwrap();
    let singular = reviewer_selection_disclosure(&params);
    assert!(singular.contains("1. default-model / default-provider; executor=native; authentication route=daemon_configured_native"));
    let roster = [
        ("native", "daemon_configured_native"),
        ("native", "daemon_configured_native"),
        ("claude_client", "claude_subscription"),
        ("claude_client", "claude_api"),
        ("native", "ollama_local"),
    ];
    params.reviewers = Some(
        roster
            .iter()
            .map(|(executor, route)| {
                serde_json::from_value(json!({"provider":"p", "model":"m",
            "executionAdapter":executor, "authenticationRoute":route}))
                .unwrap()
            })
            .collect(),
    );
    let text = reviewer_selection_disclosure(&params);
    assert!(text.contains("including queued reviewers"));
    let mut previous = 0;
    for (index, (executor, route)) in roster.iter().enumerate() {
        let at = text
            .find(&format!(
                "{}. m / p; executor={executor}; authentication route={route}",
                index + 1
            ))
            .unwrap();
        assert!(at > previous);
        previous = at;
    }
    assert_eq!(text.matches("unavailable for commissioning").count(), 2);
    assert_eq!(text.matches("have not been verified").count(), 2);
}

#[test]
fn file_lists_preserve_paths_and_disclose_every_file_and_reviewer() {
    use codex_app_server_protocol::AuditSource;
    let temp = tempfile::tempdir().unwrap();
    let cwd = temp.path();
    let paths = vec![
        "client/shared.rs".into(),
        "server/shared.rs".into(),
        "main.rs".into(),
    ];
    let source = file_source(&paths, cwd).unwrap();
    let AuditSource::FileList { files } = &source else {
        panic!("file list");
    };
    assert_eq!(
        files.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(),
        ["client/shared.rs", "server/shared.rs", "main.rs"]
    );
    let params = serde_json::from_value(serde_json::json!({
        "requestKey":"files", "source":source, "brief":"cross-file audit",
        "reviewers":[{"provider":"p","model":"first"},{"provider":"p","model":"second"}]
    }))
    .unwrap();
    let text = start_disclosure(&params);
    for file in files {
        assert!(text.contains(&file.path));
        assert!(text.contains(&format!("snapshot name {}", file.name)));
    }
    assert!(text.contains("first / p") && text.contains("second / p"));
    let mut reversed = paths.clone();
    reversed.reverse();
    let AuditSource::FileList { files: reverse } = file_source(&reversed, cwd).unwrap() else {
        panic!("file list");
    };
    assert_eq!(reverse.into_iter().rev().collect::<Vec<_>>(), *files);
    assert!(
        matches!(file_source(&paths[..1], cwd).unwrap(), AuditSource::File { name, .. } if name == "shared.rs")
    );
}

#[test]
fn model_file_list_paths_resolve_without_changing_logical_names() {
    let temp = tempfile::tempdir().unwrap();
    let mut source = serde_json::from_value(serde_json::json!({"type":"fileList","files":[
        {"name":"one/x.rs","path":"one/x.rs"}, {"name":"two/x.rs","path":"two/x.rs"}
    ]}))
    .unwrap();
    resolve_source_paths(&mut source, temp.path()).unwrap();
    let codex_app_server_protocol::AuditSource::FileList { files } = source else {
        panic!("list");
    };
    for file in files {
        assert_eq!(
            std::path::Path::new(&file.path),
            temp.path().join(&file.name)
        );
    }
}
