//! Context controls must cross the normal host-to-isolated-reviewer boundary.
use super::*;
use codex_core::config::ConfigBuilder;
use codex_core::config::ConfigOverrides;
use codex_protocol::config_types::AutoCompactTokenLimitScope;

#[tokio::test]
async fn configured_context_controls_reach_reviewer_without_ambient_instructions() {
    for scope in ["total", "body_after_prefix"] {
        let temp = tempfile::tempdir().unwrap();
        let (_, snapshot, models) = fixture(&temp).await;
        std::fs::write(
            temp.path().join("config.toml"),
            format!(
                r#"model_context_window = 100000
model_auto_compact_token_limit = 12000
model_auto_compact_token_limit_scope = "{scope}"
model_post_turn_compact_threshold_percent = 25
[features.async_compaction]
enabled = true
model_async_compact_threshold_percent = 17
"#
            ),
        )
        .unwrap();
        let mut host = ConfigBuilder::default()
            .codex_home(temp.path().to_path_buf())
            .harness_overrides(ConfigOverrides {
                cwd: Some(temp.path().to_path_buf()),
                ..Default::default()
            })
            .build()
            .await
            .unwrap();
        make_hostile(&mut host);
        assert_eq!(host.model_context_window, Some(100_000));
        assert_eq!(host.model_auto_compact_token_limit, Some(12_000));
        let prepared = prepare(&host, &models, &snapshot, request(&host), Arc::new(|_| {}))
            .await
            .unwrap();
        let config = &prepared.options.config;
        assert_eq!(config.model_context_window, Some(100_000));
        assert_eq!(config.model_auto_compact_token_limit, Some(12_000));
        assert_eq!(
            config.model_auto_compact_token_limit_scope,
            if scope == "total" {
                AutoCompactTokenLimitScope::Total
            } else {
                AutoCompactTokenLimitScope::BodyAfterPrefix
            }
        );
        assert_eq!(config.model_post_turn_compact_threshold_percent, 25);
        assert_eq!(config.async_compaction_threshold_percent, 17);
        assert!(config.features.enabled(Feature::AsyncCompaction));
        assert_eq!(prepared.policy["model_metadata"]["context_window"], 100_000);
        assert_eq!(
            prepared.policy["model_metadata"]["auto_compact_token_limit"],
            12_000
        );
        assert_eq!(config.base_instructions, None);
        assert_eq!(config.developer_instructions.as_deref(), Some(AUDIT_RULES));
        assert_eq!(config.notify, None);
        assert_eq!(config.project_doc_max_bytes, 0);
        assert_eq!(config.cwd, snapshot.source_root());
        assert!(config.mcp_servers.get().is_empty());
        assert!(!config.features.enabled(Feature::ShellTool));
        assert!(!config.features.enabled(Feature::Plugins));
    }
}

#[tokio::test]
async fn absent_context_overrides_keep_native_defaults() {
    let temp = tempfile::tempdir().unwrap();
    let (host, snapshot, models) = fixture(&temp).await;
    let expected = models
        .get_model_info("audit-test", &host.to_models_manager_config())
        .await;
    let prepared = prepare(&host, &models, &snapshot, request(&host), Arc::new(|_| {}))
        .await
        .unwrap();
    let config = &prepared.options.config;
    assert_eq!(config.model_context_window, None);
    assert_eq!(config.model_auto_compact_token_limit, None);
    assert_eq!(
        config.model_auto_compact_token_limit_scope,
        host.model_auto_compact_token_limit_scope
    );
    assert_eq!(config.model_post_turn_compact_threshold_percent, 0);
    assert_eq!(
        config.async_compaction_threshold_percent,
        host.async_compaction_threshold_percent
    );
    assert_eq!(
        prepared.policy["model_metadata"],
        serde_json::to_value(expected.model.as_ref()).unwrap()
    );
}
