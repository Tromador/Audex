use super::*;

#[tokio::test]
async fn slash_and_tool_inputs_preserve_large_brief_and_distinct_documents() {
    let temp = tempfile::tempdir().unwrap();
    let guidance = format!("{}GUIDANCE_END", "é".repeat(32_768));
    let evidence = format!("{}EVIDENCE_END", "ø".repeat(32_768));
    let brief = format!("{}BRIEF_END", "ê".repeat(32_768));
    std::fs::write(temp.path().join("AUDEX.md"), &guidance).unwrap();
    std::fs::write(temp.path().join("AGENTS.md"), &evidence).unwrap();
    let action = parse_command(&format!(
        "--snippet source --name source.rs --brief '{brief}' --audex-md AUDEX.md --agents-md AGENTS.md"
    )).unwrap().action(temp.path()).await.unwrap();
    let AuditAction::Start(args) = action else {
        panic!("start")
    };
    assert_eq!(args.brief, brief);
    assert_eq!(args.audex_guidance.as_deref(), Some(guidance.as_str()));
    assert_eq!(args.agents_evidence.as_deref(), Some(evidence.as_str()));
    let action = tool_action(
        "audit_start",
        json!({
            "source":{"type":"snippet","name":"source.rs","content":"source"},
            "brief":brief,"audexGuidance":guidance,"agentsEvidence":evidence
        }),
    )
    .unwrap();
    let AuditAction::Start(args) = action else {
        panic!("start")
    };
    assert_eq!(args.brief, brief);
    assert_eq!(args.audex_guidance.as_deref(), Some(guidance.as_str()));
    assert_eq!(args.agents_evidence.as_deref(), Some(evidence.as_str()));
    assert_eq!(document(None).await.unwrap(), None);
    assert!(document(Some(temp.path().to_owned())).await.is_err());
    assert!(document(Some(temp.path().join("absent"))).await.is_err());
    std::fs::write(temp.path().join("AUDEX.md"), [0xff]).unwrap();
    assert!(document(Some(temp.path().join("AUDEX.md"))).await.is_err());
}
