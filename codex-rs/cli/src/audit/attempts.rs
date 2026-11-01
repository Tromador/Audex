use super::*;
use codex_app_server_protocol::{
    AuditAttemptAction, AuditAttemptControlParams, AuditAttemptReadParams,
};

#[derive(Debug, Subcommand)]
pub(super) enum AttemptCommand {
    /// List current and historical attempts in reviewer/attempt order.
    List { run_id: String },
    /// Observe committed activity. Text and JSON fragments remain unvalidated.
    #[command(visible_alias = "observe")]
    Read {
        run_id: String,
        assignment_id: String,
        #[arg(long, default_value_t = 0)]
        after_sequence: i64,
        #[arg(long, default_value_t = 64)]
        limit: u32,
    },
    /// Wait for this exact attempt to complete, fail or be excluded; never follows a replacement.
    Wait {
        run_id: String,
        assignment_id: String,
    },
    /// Exclude this attempt and freeze its prefix. Retry remains possible only while collection is open.
    /// Settling the last outstanding contribution immediately closes Retry and finalises collection.
    Cancel(ControlArgs),
    /// Permanently exclude this reviewer from Retry, preserving received incomplete output.
    /// Settling the last outstanding contribution immediately finalises collection.
    Ignore(ControlArgs),
    /// Explicitly commission a fresh attempt with the same source and brief.
    Retry {
        #[command(flatten)]
        target: ControlArgs,
        #[arg(long, required = true)]
        authorise_retry: bool,
        /// The old request may remain active and incur overlapping charges.
        #[arg(long)]
        acknowledge_possible_overlap: bool,
    },
}

#[derive(Debug, Args)]
pub(super) struct ControlArgs {
    run_id: String,
    assignment_id: String,
    /// Reuse this key and all arguments if the acknowledgement is lost.
    #[arg(long)]
    action_key: Option<String>,
    #[arg(long, default_value = "operator decision")]
    reason: String,
}

pub(super) async fn run(audits: &AuditClient, command: AttemptCommand) -> anyhow::Result<i32> {
    let (target, action, authorise_retry, acknowledge_possible_overlap) = match command {
        AttemptCommand::List { run_id } => {
            println!(
                "{}",
                serde_json::to_string_pretty(&audits.attempts(run_id).await?)?
            );
            return Ok(0);
        }
        AttemptCommand::Wait {
            run_id,
            assignment_id,
        } => loop {
            let read = audits
                .read_attempt(AuditAttemptReadParams {
                    run_id: run_id.clone(),
                    assignment_id: assignment_id.clone(),
                    after_sequence: Some(0),
                    limit: Some(64),
                })
                .await?;
            let attempt = &read.attempt;
            if attempt.completed
                || !attempt.current
                || attempt.disposition != "active"
                || matches!(attempt.operation_state.as_str(), "failed" | "unknown")
            {
                println!("{}", serde_json::to_string_pretty(&read)?);
                return Ok(if attempt.completed {
                    0
                } else if attempt.disposition != "active" || !attempt.current {
                    3
                } else {
                    5
                });
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        },
        AttemptCommand::Read {
            run_id,
            assignment_id,
            after_sequence,
            limit,
        } => {
            let read = audits
                .read_attempt(AuditAttemptReadParams {
                    run_id,
                    assignment_id,
                    after_sequence: Some(after_sequence),
                    limit: Some(limit),
                })
                .await?;
            eprintln!(
                "Captured activity is unvalidated; complete messages and JSON fragments do not establish a completed review."
            );
            println!("{}", serde_json::to_string_pretty(&read)?);
            return Ok(0);
        }
        AttemptCommand::Cancel(target) => (target, AuditAttemptAction::Cancel, false, false),
        AttemptCommand::Ignore(target) => (target, AuditAttemptAction::Ignore, false, false),
        AttemptCommand::Retry {
            target,
            authorise_retry,
            acknowledge_possible_overlap,
        } => (
            target,
            AuditAttemptAction::Retry,
            authorise_retry,
            acknowledge_possible_overlap,
        ),
    };
    if action == AuditAttemptAction::Retry {
        let attempts = audits.attempts(target.run_id.clone()).await?;
        let original = attempts
            .attempts
            .iter()
            .find(|attempt| attempt.assignment_id == target.assignment_id)
            .context("reviewer attempt does not belong to this audit")?;
        if original.possible_overlap {
            eprintln!(
                "The prior request may remain active and incur overlapping charges. Retry needs --authorise-retry --acknowledge-possible-overlap; no separate capacity-release action is needed."
            );
            ensure!(
                acknowledge_possible_overlap,
                "possible overlapping charges were not acknowledged"
            );
        }
    }
    let action_key = target
        .action_key
        .unwrap_or_else(|| uuid::Uuid::now_v7().to_string());
    eprintln!(
        "Action key: {action_key}. Reuse it with the same arguments if the acknowledgement is lost."
    );
    let response = audits
        .control_attempt(AuditAttemptControlParams {
            run_id: target.run_id,
            assignment_id: target.assignment_id,
            action_key,
            action,
            reason: target.reason,
            authorise_retry,
            acknowledge_possible_overlap,
        })
        .await?;
    println!("{}", serde_json::to_string_pretty(&response)?);
    Ok(0)
}
