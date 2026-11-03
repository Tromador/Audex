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
