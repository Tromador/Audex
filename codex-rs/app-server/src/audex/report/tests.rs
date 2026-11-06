use super::*;

#[test]
fn historical_format_three_golden_inputs_keep_exact_identity_and_bytes() -> anyhow::Result<()> {
    for (raw, bytes, input_digest, content_digest) in [
        (
            include_str!("fixtures/format-3-completed.json"),
            include_bytes!("fixtures/format-3-completed.md").as_slice(),
            "26735beb7368598fb0e1e08ea212593f7b9dc50dd819022c8165277d35cd3d26",
            "1a9f5b66fae7ca361a43fec9e21f426436685cebc3dd4e462c047b230cb4126c",
        ),
        (
            include_str!("fixtures/format-3-aborted.json"),
            include_bytes!("fixtures/format-3-aborted.md").as_slice(),
            "c6dd7060c2411b2d8a6370fbf7b74db41353acb26297c6f169f2ca34aeefcaaf",
            "3be2b754611ac88d7a5d43fb936006558aad9328963e70c74e6a0aafc3646cb8",
        ),
    ] {
        let input = collection::decode(raw)?;
        assert_eq!(serde_json::to_string(&input)?, raw);
        let result = collection::render(&input)?;
        assert_eq!(result.id, format!("audex-report-v3:sha256:{input_digest}"));
        assert_eq!(result.input_digest, input_digest);
        assert_eq!(result.content_digest, content_digest);
        assert_eq!(result.bytes, bytes);
    }
    Ok(())
}

#[test]
fn historical_format_four_keeps_the_aborted_live_report_unchanged() -> anyhow::Result<()> {
    let raw = include_str!("fixtures/format-4-aborted.json");
    let input = collection::decode(raw)?;
    assert_eq!(serde_json::to_string(&input)?, raw);
    let report = collection::render(&input)?;
    assert_eq!(report.bytes, include_bytes!("fixtures/format-4-aborted.md"));
    assert_eq!(
        report.input_digest,
        "a32d45e8b6b06ebab6d4f488dc6c5b6baeaaf9c43ab553e8571896d210439f6d"
    );
    assert_eq!(
        report.content_digest,
        "43b627020c84162ff096ce272ffaf1cc5d52199268be6b8cab53c2aeb3f3c02f"
    );
    assert!(input.reviewers[0].opinion.is_none());
    assert!(input.reviewers[0].completed.is_none());
    Ok(())
}

#[test]
fn historical_format_five_keeps_completed_reports_without_tool_facts_unchanged()
-> anyhow::Result<()> {
    let raw = include_str!("fixtures/format-5-completed.json");
    let input = collection::decode(raw)?;
    assert_eq!(serde_json::to_string(&input)?, raw);
    assert!(input.reviewers.iter().all(|r| r.source_tools.is_none()));
    let report = collection::render(&input)?;
    assert_eq!(
        report.bytes,
        include_bytes!("fixtures/format-5-completed.md")
    );
    assert_eq!(
        report.input_digest,
        "f5e30e1ec7960ba7cd91fb1b30afbbacaf89ca826b9956421e201f6c2a611b28"
    );
    assert_eq!(
        report.content_digest,
        "a15362c26b91214cb9e1cbd3215d40ece68cbb252ae14f2ee001bfeaa0130407"
    );
    Ok(())
}

#[test]
fn historical_format_two_keeps_its_separate_digest_and_renderer() -> anyhow::Result<()> {
    let raw = r#"{"run_id":"old-run","snapshot_id":"old-source","assignment_id":"old-attempt","summary":"Historical summary","findings":[]}"#;
    let input: ReportInput = serde_json::from_str(raw)?;
    let expected = b"# Audex Audit Report\n\n- Report format: `2`\n- Run: `old-run`\n- Assignment: `old-attempt`\n- Snapshot: `old-source`\n\n## Summary\n\n> Historical summary\n\n## Findings\n\nNo findings were reported.\n";
    let result = render(&input)?;
    assert_eq!(result.bytes, expected);
    assert_eq!(
        result.input_digest,
        sha256_hex(format!("[2,{raw}]").as_bytes())
    );
    assert_eq!(
        result.id,
        format!("audex-report-v2:sha256:{}", result.input_digest)
    );
    assert_eq!(result.content_digest, sha256_hex(expected));
    Ok(())
}

#[test]
fn unsupported_versions_and_missing_format_four_facts_are_not_upgraded() -> anyhow::Result<()> {
    let mut value: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/format-3-completed.json"))?;
    for version in [2, 4, 5] {
        value["format_version"] = serde_json::json!(version);
        assert!(collection::decode(&value.to_string()).is_err());
    }
    Ok(())
}

/// Report freeze and whole-audit Abort decode retained usage strictly. These exhaustive
/// literals stop compilation when upstream adds a field, so the mirror is reviewed at merge.
#[test]
fn native_usage_mirror_accepts_every_upstream_usage_field() -> anyhow::Result<()> {
    use codex_protocol::protocol::TokenUsage;
    use codex_protocol::protocol::TokenUsageInfo;
    let counters = TokenUsage {
        input_tokens: 1,
        cached_input_tokens: 2,
        cache_write_input_tokens: 3,
        output_tokens: 4,
        reasoning_output_tokens: 5,
        total_tokens: 15,
        codex_rollout_budget_units: Some(serde_json::Number::from(6)),
    };
    let info = TokenUsageInfo {
        total_token_usage: counters.clone(),
        last_token_usage: counters,
        last_usage_includes_reasoning: Some(true),
        model_context_window: Some(4096),
    };
    let usage: facts::NativeUsage = serde_json::from_str(&serde_json::to_string(&info)?)?;
    assert_eq!(usage.total_token_usage.cache_write_input_tokens, Some(3));
    assert_eq!(usage.last_usage_includes_reasoning, Some(true));
    Ok(())
}
