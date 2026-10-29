use super::*;

#[tokio::test]
async fn migration_keeps_legacy_submission_uncertainty_and_positive_evidence() -> anyhow::Result<()>
{
    let temp = tempfile::tempdir()?;
    let root = AbsolutePathBuf::from_absolute_path(temp.path())?;
    let pool = open_pool(&root.join(DATABASE_FILENAME).into_path_buf()).await?;
    Migrator::with_migrations(
        AUDEX_MIGRATOR
            .iter()
            .filter(|m| m.version <= 3)
            .cloned()
            .collect(),
    )
    .run(&pool)
    .await?;
    for (name, state, turn) in [
        ("planned", "planned", None),
        ("dispatching", "dispatching", None),
        ("receipt_window", "dispatching", Some("accepted-turn")),
        ("accepted", "accepted", Some("accepted-turn")),
    ] {
        sqlx::query("INSERT INTO audit_runs(id,request_key,snapshot_id,manifest_digest,manifest_path,brief_digest,policy_digest,input_digest,phase,control_intent,revision,created_at_ms,updated_at_ms) VALUES(?,?,'snapshot','digest','manifest','brief','policy','input','reviewing','active',0,0,0)")
            .bind(name).bind(name).execute(&pool).await?;
        sqlx::query("INSERT INTO reviewer_assignments(id,run_id,ordinal,snapshot_id,provider,model,reviewer_input_digest,state,created_at_ms,updated_at_ms) VALUES(?,?,0,'snapshot','openai','test','input',?,0,0)")
            .bind(name).bind(name).bind(if state=="accepted" {"running"} else {state}).execute(&pool).await?;
        sqlx::query("INSERT INTO operations(id,assignment_id,kind,state,attempt,input_digest,created_at_ms,updated_at_ms) VALUES(?,?,'review',?,0,'input',0,0)")
            .bind(name).bind(name).bind(state).execute(&pool).await?;
        if let Some(turn) = turn {
            sqlx::query("INSERT INTO audit_execution(operation_id,thread_id,turn_id,policy_digest) VALUES(?,?,?,'policy')")
                .bind(name).bind(name).bind(turn).execute(&pool).await?;
        }
    }
    pool.close().await;
    let store = AuditStore::open(root).await?;
    assert_eq!(
        store.submission_boundary("planned").await?,
        SubmissionBoundary::Unsubmitted
    );
    assert_eq!(
        store.submission_boundary("dispatching").await?,
        SubmissionBoundary::LegacyUncertain
    );
    assert_eq!(
        store.submission_boundary("receipt_window").await?,
        SubmissionBoundary::Accepted
    );
    assert_eq!(
        store.submission_boundary("accepted").await?,
        SubmissionBoundary::Accepted
    );
    assert_eq!(
        store
            .execution("receipt_window")
            .await?
            .expect("preserved binding")
            .1
            .as_deref(),
        Some("accepted-turn")
    );
    store.close().await;
    Ok(())
}
