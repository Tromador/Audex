use super::*;

async fn legacy(root: &AbsolutePathBuf, phase: &str) -> anyhow::Result<SqlitePool> {
    let pool = open_pool(&root.join(DATABASE_FILENAME).into_path_buf()).await?;
    Migrator::with_migrations(
        AUDEX_MIGRATOR
            .iter()
            .filter(|m| m.version <= 7)
            .cloned()
            .collect(),
    )
    .run(&pool)
    .await?;
    sqlx::query("INSERT INTO audit_runs(id,request_key,snapshot_id,manifest_digest,manifest_path,brief_digest,policy_digest,input_digest,phase,control_intent,revision,created_at_ms,updated_at_ms) VALUES('legacy','legacy-key','snapshot','digest','manifest','brief','policy','input',?,'active',7,0,0)")
        .bind(phase).execute(&pool).await?;
    sqlx::query("INSERT INTO reviewer_assignments(id,run_id,ordinal,snapshot_id,provider,model,reviewer_input_digest,state,created_at_ms,updated_at_ms) VALUES('legacy-assignment','legacy',0,'snapshot','legacy-provider','legacy-model','input','succeeded',0,0)")
        .execute(&pool).await?;
    sqlx::query("INSERT INTO operations(id,assignment_id,kind,state,attempt,input_digest,created_at_ms,updated_at_ms) VALUES('legacy-operation','legacy-assignment','review','committed',1,'input',0,0)")
        .execute(&pool).await?;
    Ok(pool)
}

#[tokio::test]
async fn terminal_legacy_report_bytes_and_identity_survive_upgrade() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let root = AbsolutePathBuf::from_absolute_path(temp.path())?;
    let pool = legacy(&root, "completed").await?;
    let bytes = b"Legacy immutable report: no conversion.\n";
    let digest = sha256_hex(bytes);
    publish_immutable(root.as_path(), "reports/legacy.md", bytes, &digest)?;
    sqlx::query("INSERT INTO reports(id,run_id,format_version,input_digest,relative_path,content_digest,created_at_ms) VALUES('legacy-report','legacy',2,'input','reports/legacy.md',?,0)")
        .bind(&digest).execute(&pool).await?;
    sqlx::query("UPDATE audit_runs SET current_report_id='legacy-report' WHERE id='legacy'")
        .execute(&pool)
        .await?;
    pool.close().await;
    let store = AuditStore::open(root.clone()).await?;
    assert_eq!(
        store.applied_migrations().await?,
        vec![1, 2, 3, 4, 5, 6, 7, 8]
    );
    let record = store.load_run("legacy").await?;
    assert_eq!(record.run_id, "legacy");
    assert_eq!(record.assignment_id, "legacy-assignment");
    assert_eq!(record.operation_id, "legacy-operation");
    assert_eq!(record.reviewer_ordinal, 0);
    assert_eq!(record.attempt_ordinal, 1);
    assert_eq!(record.attempt_revision, 7);
    assert_eq!(record.provider, "legacy-provider");
    assert_eq!(record.model, "legacy-model");
    assert_eq!(
        store.read_report(record.report.as_ref().expect("legacy report"))?,
        bytes
    );
    assert_eq!(
        store.attempts("legacy").await?,
        vec![store.load_run_metadata("legacy").await?]
    );
    store.close().await;
    let reopened = AuditStore::open(root).await?;
    assert_eq!(reopened.load_run("legacy").await?, record);
    Ok(())
}

#[tokio::test]
async fn nonterminal_legacy_upgrade_refuses_without_changing_the_ledger() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let root = AbsolutePathBuf::from_absolute_path(temp.path())?;
    let pool = legacy(&root, "reviewing").await?;
    let before: Vec<(String, String, i64)> =
        sqlx::query_as("SELECT id,phase,revision FROM audit_runs")
            .fetch_all(&pool)
            .await?;
    pool.close().await;
    let error = match AuditStore::open(root.clone()).await {
        Err(error) => error,
        Ok(_) => panic!("unfinished legacy work must be refused"),
    };
    assert!(error.to_string().contains("non-terminal legacy audit"));
    let pool = open_pool(&root.join(DATABASE_FILENAME).into_path_buf()).await?;
    let after: Vec<(String, String, i64)> =
        sqlx::query_as("SELECT id,phase,revision FROM audit_runs")
            .fetch_all(&pool)
            .await?;
    assert_eq!(after, before);
    let versions: Vec<i64> =
        sqlx::query_scalar("SELECT version FROM _sqlx_migrations ORDER BY version")
            .fetch_all(&pool)
            .await?;
    assert_eq!(versions, vec![1, 2, 3, 4, 5, 6, 7]);
    let added: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='audit_reviewers')",
    )
    .fetch_one(&pool)
    .await?;
    assert!(!added);
    // The transactional migration guard also refuses an upgrade without preflight.
    assert!(AUDEX_MIGRATOR.run(&pool).await.is_err());
    let added: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='audit_reviewers')",
    )
    .fetch_one(&pool)
    .await?;
    assert!(!added);
    Ok(())
}
