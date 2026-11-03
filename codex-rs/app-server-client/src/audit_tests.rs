use super::*;
use serde_json::json;

#[test]
fn protocol_six_rejects_five_and_newer_without_fallback() {
    assert_eq!(AUDIT_PROTOCOL_VERSION, 6);
    assert!(require_audit_protocol_version(6).is_ok());
    for version in [0, 5, 7] {
        let error = require_audit_protocol_version(version).unwrap_err();
        assert!(!error.start_acknowledgement_uncertain());
        assert!(error.retained_run_id().is_none());
        assert!(error.to_string().contains("requires 6"));
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
    assert_eq!(text.matches("unavailable for commissioning").count(), 3);
    assert_eq!(text.matches("have not been verified").count(), 3);
}
