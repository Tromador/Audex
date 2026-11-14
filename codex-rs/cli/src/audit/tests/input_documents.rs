use super::*;

#[tokio::test]
async fn both_explicit_document_roles_are_complete_above_64_kib() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let guidance = format!("{}GUIDANCE_END", "é".repeat(32_768));
    let evidence = format!("{}EVIDENCE_END", "ø".repeat(32_768));
    let audex = temp.path().join("AUDEX.md");
    let agents = temp.path().join("AGENTS.md");
    std::fs::write(&audex, &guidance)?;
    std::fs::write(&agents, &evidence)?;
    let command = AuditCommand::try_parse_from([
        "audit",
        "--file",
        "source.rs",
        "--audex-md",
        audex.to_str().unwrap(),
        "--agents-md",
        agents.to_str().unwrap(),
    ])?;
    assert_eq!(document(command.audex_md.as_deref()).await?, Some(guidance));
    assert_eq!(
        document(command.agents_md.as_deref()).await?,
        Some(evidence)
    );
    assert_eq!(document(None).await?, None);
    assert!(document(Some(temp.path())).await.is_err());
    assert!(document(Some(&temp.path().join("absent"))).await.is_err());
    std::fs::write(&audex, [0xff])?;
    assert!(document(Some(&audex)).await.is_err());
    Ok(())
}
