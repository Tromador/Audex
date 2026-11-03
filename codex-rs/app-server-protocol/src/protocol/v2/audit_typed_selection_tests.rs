use super::*;
use serde_json::json;

#[test]
fn exact_typed_routes_roundtrip_without_inference() {
    for (executor, route) in [
        ("native", "daemon_configured_native"),
        ("claude_client", "claude_subscription"),
        ("claude_client", "claude_api"),
        ("native", "ollama_local"),
    ] {
        let wire = json!({"provider":"independent-provider", "model":"independent-model",
            "executionAdapter":executor, "authenticationRoute":route});
        let selection: AuditReviewerSelection = serde_json::from_value(wire).unwrap();
        assert_eq!(selection.validate(), Ok(()));
        assert_eq!(selection.execution_adapter.to_string(), executor);
        assert_eq!(selection.authentication_route.to_string(), route);
        let encoded = serde_json::to_value(&selection).unwrap();
        assert_eq!(encoded["executionAdapter"], executor);
        assert_eq!(encoded["authenticationRoute"], route);
        assert_eq!(encoded["provider"], "independent-provider");
        assert_eq!(encoded["model"], "independent-model");
        assert_eq!(
            serde_json::from_value::<AuditReviewerSelection>(encoded).unwrap(),
            selection
        );
    }
}

#[test]
fn omitted_fields_mean_only_daemon_configured_native() {
    let selection: AuditReviewerSelection =
        serde_json::from_value(json!({"provider":"p", "model":"m"})).unwrap();
    assert_eq!(selection.execution_adapter, AuditExecutionAdapter::Native);
    assert_eq!(
        selection.authentication_route,
        AuditAuthenticationRoute::DaemonConfiguredNative
    );
    assert_eq!(selection.validate(), Ok(()));
    for wire in [
        json!({"provider":"p", "model":"m", "executionAdapter":"claude_client"}),
        json!({"provider":"p", "model":"m", "authenticationRoute":"claude_api"}),
    ] {
        assert!(
            serde_json::from_value::<AuditReviewerSelection>(wire)
                .unwrap()
                .validate()
                .is_err()
        );
    }
}

#[test]
fn incompatible_pairs_and_malformed_selections_are_rejected() {
    for (executor, route) in [
        ("native", "claude_subscription"),
        ("native", "claude_api"),
        ("claude_client", "daemon_configured_native"),
        ("claude_client", "ollama_local"),
    ] {
        let selection: AuditReviewerSelection = serde_json::from_value(json!({
            "provider":"p", "model":"m", "executionAdapter":executor, "authenticationRoute":route
        }))
        .unwrap();
        assert_eq!(
            selection.validate(),
            Err("incompatible reviewer executor and authentication route")
        );
    }
    for wire in [
        json!({}),
        json!({"provider":"p"}),
        json!({"model":"m"}),
        json!({"provider":"p", "model":"m", "executionAdapter":"unknown"}),
        json!({"provider":"p", "model":"m", "authenticationRoute":"unknown"}),
        json!({"provider":"p", "model":"m", "authenticationRoute":null}),
        json!({"provider":"p", "model":"m", "executionAdapter":null}),
        json!({"provider":"p", "model":5}),
        json!({"provider":"p", "model":"m", "route":"claude_api"}),
    ] {
        assert!(
            serde_json::from_value::<AuditReviewerSelection>(wire.clone()).is_err(),
            "{wire}"
        );
    }
    for wire in [
        json!({"provider":" ", "model":"m"}),
        json!({"provider":"p", "model":""}),
        json!({"provider":"p", "model":"m", "expectedServedModel":""}),
    ] {
        assert!(
            serde_json::from_value::<AuditReviewerSelection>(wire)
                .unwrap()
                .validate()
                .is_err()
        );
    }
}
