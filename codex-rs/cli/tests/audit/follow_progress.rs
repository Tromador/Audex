//! Real-daemon reads of controlled durable attempt transitions.
use super::*;
use tokio::io::{AsyncBufReadExt, BufReader, Lines};
use tokio::process::ChildStderr;

async fn progress_line(
    lines: &mut Lines<BufReader<ChildStderr>>,
    expected: &str,
) -> Result<String> {
    let mut received = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(line) = lines.next_line().await? {
            if line.contains(expected) {
                return Ok(line);
            }
            received.push(line);
        }
        anyhow::bail!("follow closed before {expected:?}: {received:?}")
    })
    .await
    .with_context(|| format!("follow did not display {expected:?}: {received:?}"))?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn follow_displays_attempt_progress_without_a_run_revision_increment() -> Result<()> {
    let server = responses::start_mock_server().await;
    provider(&server, "blocked").await;
    let daemon = configured(&server)?;
    let source = daemon.home.path().join("example.rs");
    std::fs::write(&source, b"fn main() {}\n")?;
    daemon.lifecycle("start")?;
    let submitted = audit(&daemon)
        .arg("--file")
        .arg(&source)
        .arg("--detach")
        .output()?;
    check(&submitted, 0)?;
    let id = String::from_utf8(submitted.stdout)?.trim().to_owned();
    request_seen(&server).await?;
    daemon.lifecycle("stop")?;
    let pool = fixture_db(&daemon).await?;
    let (assignment, operation): (String, String) = sqlx::query_as(
        "SELECT a.id,o.id FROM reviewer_assignments a JOIN operations o ON o.assignment_id=a.id WHERE a.run_id=?",
    ).bind(&id).fetch_one(&pool).await?;
    // Keep execution owned by this fixture while the real daemon serves reads.
    // Only the fixture advances attempt state; no reviewer can be resubmitted.
    let mut tx = pool.begin().await?;
    sqlx::query("UPDATE operations SET state='dispatching',lease_owner='follow-fixture',lease_expires_at_ms=? WHERE id=?")
        .bind(i64::MAX).bind(&operation).execute(&mut *tx).await?;
    sqlx::query("UPDATE reviewer_assignments SET state='dispatching' WHERE id=?")
        .bind(&assignment)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE audit_attempts SET revision=revision+1 WHERE assignment_id=?")
        .bind(&assignment)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    let revision: i64 = sqlx::query_scalar("SELECT revision FROM audit_runs WHERE id=?")
        .bind(&id)
        .fetch_one(&pool)
        .await?;
    let attempt_revision: i64 =
        sqlx::query_scalar("SELECT revision FROM audit_attempts WHERE assignment_id=?")
            .bind(&assignment)
            .fetch_one(&pool)
            .await?;
    daemon.lifecycle("start")?;
    let mut follower = tokio::process::Command::from(audit(&daemon))
        .args(["read", &id, "--follow"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let mut lines = BufReader::new(follower.stderr.take().context("follow stderr")?).lines();
    let dispatching = progress_line(&mut lines, "reviewing / dispatching").await?;
    assert!(dispatching.contains(&format!("(revision {revision})")));

    let mut tx = pool.begin().await?;
    sqlx::query("UPDATE operations SET state='accepted' WHERE id=?")
        .bind(&operation)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE reviewer_assignments SET state='running' WHERE id=?")
        .bind(&assignment)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE audit_attempts SET revision=revision+1 WHERE assignment_id=?")
        .bind(&assignment)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    let accepted = progress_line(&mut lines, "reviewing / accepted").await?;
    assert!(accepted.contains(&format!("(revision {revision})")));
    assert!(!accepted.contains("observation pending"));

    let mut tx = pool.begin().await?;
    sqlx::query("UPDATE operations SET retry_classification='observe_only',error='fixture observation unavailable' WHERE id=?")
        .bind(&operation).execute(&mut *tx).await?;
    sqlx::query("UPDATE audit_attempts SET revision=revision+1 WHERE assignment_id=?")
        .bind(&assignment)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    let pending = progress_line(&mut lines, "observation pending").await?;
    assert!(pending.contains("reviewing / accepted"));
    assert!(pending.contains(&format!("(revision {revision})")));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT revision FROM audit_runs WHERE id=?")
            .bind(&id)
            .fetch_one(&pool)
            .await?,
        revision
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT revision FROM audit_attempts WHERE assignment_id=?")
            .bind(&assignment)
            .fetch_one(&pool)
            .await?,
        attempt_revision + 2
    );
    assert_eq!(
        server.received_requests().await.context("requests")?.len(),
        1
    );
    signal(follower.id().context("follow PID")?, libc::SIGINT)?;
    let exit = tokio::time::timeout(Duration::from_secs(5), follower.wait()).await??;
    assert_eq!(exit.code(), Some(130));
    pool.close().await;
    Ok(())
}
