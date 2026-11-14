use super::*;
use crate::audex::retained::RetainedInput;

#[tokio::test]
async fn c51_database_upgrade_preserves_native_retained_bytes_and_reopens() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let root = AbsolutePathBuf::from_absolute_path(temp.path())?;
    let database_path = root.join(DATABASE_FILENAME).into_path_buf();
    let pool = open_pool(&database_path).await?;
    Migrator::with_migrations(
        AUDEX_MIGRATOR
            .iter()
            .filter(|m| m.version <= 12)
            .cloned()
            .collect(),
    )
    .run(&pool)
    .await?;
    let old = AuditStore {
        root: root.clone(),
        database_path,
        pool,
        transition_barrier: Default::default(),
    };
    let mut fixtures = Vec::new();
    for (ordinal,raw) in [
        r#"{"version":1,"brief":"Original brief","provider":"openai","model":"requested","reasoningEffort":null}"#,
        r#"{"version":1,"brief":"Original brief","provider":"openai","model":"requested","reasoningEffort":"high","requireServedIdentity":true,"resolvedModel":"resolved","expectedServedModel":"served"}"#,
        r#"{"version":1,"brief":"Original brief","provider":"openai","model":"requested","reasoningEffort":null,"requireServedIdentity":true,"resolvedModel":"resolved","expectedServedModel":null,"executionAdapter":"native","authenticationRoute":"daemon_configured_native"}"#,
    ].into_iter().enumerate() {
        let policy=r#"{"version":1,"source":"original native policy"}"#;
        let digest=sha256_hex(raw.as_bytes());
        let request=CreateRunRequest {
            request_key:format!("legacy-{ordinal}"),snapshot_id:format!("audex-source-v1:sha256:{}",sha256_hex(b"snapshot")),
            manifest_digest:sha256_hex(b"manifest"),manifest_path:"snapshots/original/manifest.json".into(),brief_digest:sha256_hex(b"Original brief"),
            policy_digest:sha256_hex(policy.as_bytes()),input_digest:digest.clone(),provider:"openai".into(),model:"requested".into(),reasoning_effort:None,
            reviewer_input_digest:sha256_hex(b"reviewer"),operation_input_digest:digest,
        };
        let identity=sha256_hex(format!("legacy-identity-{ordinal}").as_bytes());
        let run=old.create_run_with_input(&request,Some((raw,policy,&identity))).await?;
        fixtures.push((request,identity,run,raw.to_owned(),policy.to_owned()));
    }
    old.close().await;
    let upgraded = AuditStore::open(root.clone()).await?;
    assert_eq!(
        upgraded.applied_migrations().await?,
        (1..=18).collect::<Vec<_>>()
    );
    for (request, identity, run, raw, policy) in &fixtures {
        let input = upgraded.attempt_input(&run.operation_id).await?;
        assert_eq!(&input.0, raw);
        assert_eq!(&input.1, policy);
        assert_eq!(input.2, request.policy_digest);
        assert_eq!(
            upgraded
                .find_idempotent_request(&request.request_key, identity)
                .await?,
            Some(run.clone())
        );
        let decoded = RetainedInput::decode(raw)?;
        decoded.require_native()?;
        assert_eq!(decoded.brief, "Original brief");
        assert_eq!(decoded.provider, "openai");
        assert_eq!(decoded.model, "requested");
        assert!(decoded.bindings.is_none());
    }
    upgraded.close().await;
    let reopened = AuditStore::open(root).await?;
    for (_, _, run, raw, _) in fixtures {
        assert_eq!(reopened.attempt_input(&run.operation_id).await?.0, raw);
    }
    Ok(())
}

#[tokio::test]
async fn request_key_probe_during_schema_creation_does_not_migrate_or_fail() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let root = AbsolutePathBuf::from_absolute_path(temp.path())?;
    assert!(!AuditStore::request_key_exists_read_only(&root, "missing").await?);
    assert!(!root.join(DATABASE_FILENAME).exists());
    let pool = open_pool(&root.join(DATABASE_FILENAME).into_path_buf()).await?;
    assert!(!AuditStore::request_key_exists_read_only(&root, "missing").await?);
    let tables: i64 = sqlx::query_scalar("SELECT count(*) FROM sqlite_master WHERE type='table'")
        .fetch_one(&pool)
        .await?;
    assert_eq!(tables, 0, "the preflight probe must remain read-only");
    pool.close().await;
    Ok(())
}
