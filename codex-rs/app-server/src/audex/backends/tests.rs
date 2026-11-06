use super::*;
use serde_json::json;

#[test]
fn route_identity_binds_schema_version_and_validates_unrelated_sections() -> anyhow::Result<()> {
    // The app-server test build uses the same JSON object ordering as the CLI.
    let settings = BackendSettings::default();
    for (route, canonical) in [
        (
            AuditAuthenticationRoute::DaemonConfiguredNative,
            r#"{"version":1}"#,
        ),
        (
            AuditAuthenticationRoute::ClaudeSubscription,
            r#"{"version":1,"claudeSubscription":null}"#,
        ),
        (
            AuditAuthenticationRoute::ClaudeApi,
            r#"{"version":1,"claudeApi":null}"#,
        ),
        (
            AuditAuthenticationRoute::OllamaLocal,
            r#"{"version":1,"ollamaLocal":null}"#,
        ),
    ] {
        assert_eq!(settings.identity(&route)?, sha256_hex(canonical.as_bytes()));
        let mut unsupported = settings.clone();
        unsupported.version = 2;
        assert!(unsupported.identity(&route).is_err());
        let malformed: BackendSettings = serde_json::from_value(json!({"version":1,
            "claudeSubscription":{"executable":"/absent/claude","clientVersion":"fixture",
                "profile":"relative","runtimeRoot":"/absent/private"}}))?;
        assert!(malformed.identity(&route).is_err());
    }
    Ok(())
}

#[test]
fn strict_settings_identity_defaults_and_non_secret_references() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("reviewer-backends.json");
    let route = AuditAuthenticationRoute::ClaudeApi;
    let missing = BackendSettings::load(temp.path())?;
    assert_eq!(missing, BackendSettings::default());
    std::fs::write(&path, r#"{ "version": 1 }"#)?;
    assert_eq!(
        BackendSettings::load(temp.path())?.identity(&route)?,
        missing.identity(&route)?
    );
    let settings = json!({"version":1,"claudeApi":{
        "executable":"/absent/claude", "clientVersion":"2.1.314", "runtimeRoot":"/absent/private",
        "credential":{"kind":"file","reference":"/absent/credentials"},"upstream":"https://api.example.test"}});
    std::fs::write(&path, serde_json::to_vec_pretty(&settings)?)?;
    let loaded = BackendSettings::load(temp.path())?;
    assert!(loaded.claude_subscription.is_none() && loaded.ollama_local.is_none());
    assert_ne!(loaded.identity(&route)?, missing.identity(&route)?);
    std::fs::write(&path, serde_json::to_vec(&settings)?)?;
    assert_eq!(
        loaded.identity(&route)?,
        BackendSettings::load(temp.path())?.identity(&route)?
    );
    for invalid in [
        json!({"version":2}),
        json!({"version":1,"apiKey":"SECRET_SENTINEL"}),
        json!({"version":1,"claudeApi":{}}),
        json!({"version":1,"claudeSubscription":{"executable":"relative"}}),
        {
            let mut s = settings.clone();
            s["claudeApi"]["credential"]["value"] = json!("SECRET_SENTINEL");
            s
        },
        {
            let mut s = settings.clone();
            s["claudeApi"]["upstream"] = json!("https://user:SECRET_SENTINEL@example.test");
            s
        },
        {
            let mut s = settings.clone();
            s["claudeApi"]["credential"] = json!({"kind":"environment","reference":"1INVALID"});
            s
        },
    ] {
        std::fs::write(&path, serde_json::to_vec(&invalid)?)?;
        let error = BackendSettings::load(temp.path())
            .expect_err("invalid configuration")
            .to_string();
        assert!(!error.contains("SECRET_SENTINEL"));
    }
    for malformed in ["{", r#"{"version":1,"version":1}"#, "null"] {
        std::fs::write(&path, malformed)?;
        assert!(BackendSettings::load(temp.path()).is_err());
    }
    Ok(())
}
