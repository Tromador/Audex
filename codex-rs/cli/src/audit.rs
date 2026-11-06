//! Operator client only. Snapshot capture, inference and workflow authority stay
//! in the existing process-scoped app-server controller.
mod attempts;
use anyhow::{Context, bail, ensure};
use clap::{ArgMatches, Args, Parser, Subcommand, parser::ValueSource};
use codex_app_server_client::audit::AuditClient;
use codex_app_server_client::{
    AppServerEvent, RemoteAppServerClient, RemoteAppServerConnectArgs, RemoteAppServerEndpoint,
    app_server_control_socket_path, verified_local_daemon_socket,
};
use codex_app_server_protocol::{
    AuditListParams, AuditOperationState, AuditRunPhase, AuditRunView, AuditSource,
    AuditStartParams, ClientRequest, ConfigReadParams, ConfigReadResponse, JSONRPCErrorError,
    RequestId, ServerNotification,
};
use codex_core::config::find_codex_home;
use codex_utils_absolute_path::AbsolutePathBuf;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

// Client allocation ceilings mirror snapshot v1. The server still enforces its
// own authoritative capture limits, regular-file checks and snapshot verification.
const SOURCE_BYTES: usize = 32 * 1024 * 1024;
const DOCUMENT_BYTES: usize = 64 * 1024;
const PENDING: i32 = 5;

#[derive(Debug, Parser)]
#[command(
    subcommand_negates_reqs = true,
    args_conflicts_with_subcommands = true,
    after_help = "Requires an already-running local app-server daemon. Reports go to stdout; progress goes to stderr.\nRuntime exit codes: 0 completed (or acknowledged --detach), 1 request/local error, 2 failed audit, 3 cancelled, 4 terminal unknown, 5 pending, 6 aborted by operator, 130 Ctrl-C.\nCtrl-C disconnects without cancelling. Use `audit abort RUN` to stop the whole audit."
)]
pub(crate) struct AuditCommand {
    #[command(flatten)]
    source: SourceArgs,
    /// Logical filename for pasted source.
    #[arg(long, requires = "stdin", conflicts_with = "file")]
    name: Option<String>,
    /// Reviewer model. Uses the daemon's declared value, or requires this flag.
    #[arg(long)]
    model: Option<String>,
    /// Ordered selection, repeated for each reviewer. Accepts native PROVIDER:MODEL shorthand or a JSON AuditReviewerSelection.
    #[arg(long = "reviewer", conflicts_with_all = ["model", "provider", "expected_served_model"])]
    reviewers: Vec<String>,
    /// Run at most one reviewer from this audit at a time.
    #[arg(long)]
    sequential: bool,
    /// Explicitly qualify the served-model alias for the singular reviewer.
    #[arg(long)]
    expected_served_model: Option<String>,
    /// Reviewer provider. Uses the daemon's declared value, or requires this flag.
    #[arg(long)]
    provider: Option<String>,
    /// Audit priorities, submitted as the ordinary audit brief.
    #[arg(
        long,
        default_value = "Find concrete defects supported by evidence in this source."
    )]
    brief: String,
    /// Explicit, untrusted project audit guidance; never discovered automatically.
    #[arg(long)]
    audex_md: Option<PathBuf>,
    /// Explicit evidence of developer conventions; not auditor instructions.
    #[arg(long)]
    agents_md: Option<PathBuf>,
    /// Return the durable run ID instead of following progress.
    #[arg(long)]
    detach: bool,
    /// Idempotency key. Reuse the same key and input if start acknowledgement is lost.
    #[arg(long)]
    request_key: Option<String>,
    #[command(subcommand)]
    action: Option<Action>,
}

#[derive(Debug, Args)]
#[group(required = true, multiple = false)]
struct SourceArgs {
    /// Capture this regular file on the local daemon; source bytes and mode are preserved.
    #[arg(long)]
    file: Option<PathBuf>,
    /// Read up to 32 MiB of UTF-8 source from stdin; requires --name.
    #[arg(long, requires = "name")]
    stdin: bool,
}

#[derive(Debug, Subcommand)]
enum Action {
    /// Control one reviewer attempt or read its retained opinion and activity.
    Attempt {
        #[command(subcommand)]
        action: attempts::AttemptCommand,
    },
    /// Read a durable run, writing its Markdown report to stdout when available.
    Read {
        run_id: String,
        /// Keep observing through recoverable pending states; Ctrl-C only disconnects.
        #[arg(long)]
        follow: bool,
    },
    /// List durable run summaries, newest first, without opening reports or results.
    List {
        #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u32).range(1..=100))]
        limit: u32,
        /// Cursor returned by the previous page; continues towards older audits.
        #[arg(long)]
        after: Option<String>,
    },
    /// Abort the whole audit, preserve accepted reviews and eligible partials, then follow publication.
    #[command(visible_alias = "cancel")]
    Abort { run_id: String },
    /// Release an observation-pending run after cancellation; inference may still run or incur charges.
    ReleaseSlot {
        run_id: String,
        #[arg(long, required = true)]
        acknowledge_concurrent_execution_and_charges: bool,
        #[arg(long)]
        reason: String,
    },
}

pub(super) fn validate_options(root: &ArgMatches) -> Result<(), clap::Error> {
    if root.subcommand_name() == Some("audit") {
        for id in root.ids() {
            if root.value_source(id.as_str()) == Some(ValueSource::CommandLine) {
                return Err(clap::Error::raw(
                    clap::error::ErrorKind::ArgumentConflict,
                    format!(
                        "`audit` uses the running daemon's configuration; root option `{id}` is not supported. Put reviewer selection after `audit --model MODEL --provider PROVIDER`."
                    ),
                ));
            }
        }
    }
    Ok(())
}

async fn bounded_text(reader: impl AsyncRead + Unpin, limit: usize) -> anyhow::Result<String> {
    let mut bytes = Vec::new();
    reader
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .await?;
    ensure!(bytes.len() <= limit, "input exceeds the {limit}-byte limit");
    String::from_utf8(bytes).context("input must be UTF-8")
}

async fn document(path: Option<&Path>) -> anyhow::Result<Option<String>> {
    let Some(path) = path else { return Ok(None) };
    ensure!(
        tokio::fs::metadata(path).await?.is_file(),
        "context document must be a regular file"
    );
    bounded_text(tokio::fs::File::open(path).await?, DOCUMENT_BYTES)
        .await
        .with_context(|| format!("reading context document {}", path.display()))
        .map(Some)
}

async fn connect() -> anyhow::Result<RemoteAppServerClient> {
    let home = find_codex_home()?;
    let advertised = app_server_control_socket_path(&home)?;
    let hint = "No verified local app-server daemon is available. Start it with `audex app-server daemon start`, then rerun this command. Audit does not start or update services.";
    let socket_path = verified_local_daemon_socket(advertised.as_path())
        .context(hint)?
        .context(hint)?;
    #[cfg(windows)]
    let (socket_path, _directory) = {
        // Match the TUI's implicit-daemon path: keep the private-directory
        // handle alive through connection and peer-token verification.
        let (path, directory) = codex_uds::validate_private_socket_path(socket_path.as_path())?;
        (
            AbsolutePathBuf::from_absolute_path_checked(path)?,
            directory,
        )
    };
    let args = RemoteAppServerConnectArgs {
        required_human_approval: false,
        endpoint: RemoteAppServerEndpoint::UnixSocket { socket_path },
        client_name: "audex-cli".to_string(),
        client_version: env!("CARGO_PKG_VERSION").to_string(),
        experimental_api: true,
        mcp_server_openai_form_elicitation: false,
        opt_out_notification_methods: Vec::new(),
        channel_capacity: 32,
    };
    #[cfg(windows)]
    let client = RemoteAppServerClient::connect_local_daemon(args).await;
    #[cfg(not(windows))]
    let client = RemoteAppServerClient::connect(args).await;
    client.context(hint)
}

fn parse_reviewer(
    value: &str,
) -> anyhow::Result<codex_app_server_protocol::AuditReviewerSelection> {
    if value.trim_start().starts_with('{') {
        let reviewer: codex_app_server_protocol::AuditReviewerSelection =
            serde_json::from_str(value).context("invalid reviewer JSON")?;
        reviewer.validate().map_err(anyhow::Error::msg)?;
        return Ok(reviewer);
    }
    let (provider, model) = value
        .split_once(':')
        .context("--reviewer requires PROVIDER:MODEL or reviewer JSON")?;
    ensure!(
        !provider.trim().is_empty() && !model.trim().is_empty(),
        "reviewer provider and model must be nonempty"
    );
    Ok(codex_app_server_protocol::AuditReviewerSelection {
        provider: provider.to_owned(),
        model: model.to_owned(),
        reasoning_effort: None,
        execution_adapter: Default::default(),
        authentication_route: Default::default(),
        expected_served_model: None,
    })
}

fn id(name: &str) -> RequestId {
    RequestId::String(format!("audex-cli-{name}"))
}

pub(crate) async fn run(command: AuditCommand) -> anyhow::Result<i32> {
    // Read explicit input before connecting; no project discovery or source execution.
    let source = if command.action.is_none() {
        Some(if let Some(path) = &command.source.file {
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .context("file needs a UTF-8 filename")?
                .to_string();
            // Lexical absolute resolution preserves symlinks for the server to reject.
            let absolute = AbsolutePathBuf::relative_to_current_dir(path)?;
            let path = absolute
                .to_str()
                .context("source path must be UTF-8")?
                .to_string();
            AuditSource::File { name, path }
        } else {
            AuditSource::Snippet {
                name: command.name.clone().context("--stdin requires --name")?,
                content: bounded_text(tokio::io::stdin(), SOURCE_BYTES).await?,
            }
        })
    } else {
        None
    };
    let guidance = document(command.audex_md.as_deref()).await?;
    let evidence = document(command.agents_md.as_deref()).await?;
    let mut client = connect().await?;
    let mut retained_run = None;
    let result = {
        tokio::select! {
            result = execute(&mut client, command, source, guidance, evidence, &mut retained_run) => result,
            signal = tokio::signal::ctrl_c() => {
                signal?;
                eprintln!("Disconnected from daemon. Ctrl-C does not cancel an audit; use audit list/read to recover submitted work.");
                Ok(130)
            }
        }
    };
    if let Some(run) = retained_run
        && match &result {
            Err(_) => true,
            Ok(code) => *code == 130 || *code == PENDING,
        }
    {
        eprintln!("Retrieve later: audex audit read {run} --follow");
    }
    // For the remote client this closes only this connection, never the daemon.
    let _ = client.shutdown().await;
    result
}

async fn execute(
    client: &mut RemoteAppServerClient,
    command: AuditCommand,
    source: Option<AuditSource>,
    audex_guidance: Option<String>,
    agents_evidence: Option<String>,
    retained_run: &mut Option<String>,
) -> anyhow::Result<i32> {
    let requests = client.request_handle();
    let audits = AuditClient::new(codex_app_server_client::AppServerRequestHandle::Remote(
        requests.clone(),
    ));
    if !matches!(&command.action, Some(Action::List { .. })) {
        // Package version 0.0.0 cannot identify development daemon capabilities.
        // An old/mismatched response must fail before audit/start or audit/cancel.
        audits.check_compatibility().await.context(
            "Audex daemon compatibility check failed; no audit start or cancellation was submitted",
        )?;
    }
    let (view, follow) = match command.action {
        Some(Action::Attempt { action }) => return attempts::run(&audits, action).await,
        Some(Action::List { limit, after }) => {
            let response = audits
                .list(AuditListParams {
                    after,
                    limit: Some(limit),
                })
                .await?;
            for run in response.runs {
                if run.capacity_released {
                    eprintln!(
                        "{}: capacity released by operator; this does not establish execution outcome",
                        run.run_id
                    );
                }
                println!(
                    "{}\t{}\t{}\trevision={}{}",
                    run.run_id,
                    phase_name(&run.phase),
                    operation_name(&run.operation_state),
                    run.revision,
                    if run.observation_pending {
                        "\tobservation_pending"
                    } else {
                        ""
                    }
                );
            }
            if let Some(next) = response.next_cursor {
                eprintln!("Next page: audex audit list --after {next}");
            }
            return Ok(0);
        }
        Some(Action::Read { run_id, follow }) => {
            *retained_run = Some(run_id.clone());
            (audits.read(run_id).await?, follow)
        }
        Some(Action::Abort { run_id }) => {
            *retained_run = Some(run_id.clone());
            // The server commits intent before returning; this client never sends
            // a thread interrupt or claims cancellation from an attempted request.
            (audits.cancel(run_id).await?, true)
        }
        Some(Action::ReleaseSlot {
            run_id,
            acknowledge_concurrent_execution_and_charges,
            reason,
        }) => {
            ensure!(
                acknowledge_concurrent_execution_and_charges,
                "explicit acknowledgement is required"
            );
            let current = audits.read(run_id.clone()).await?;
            *retained_run = Some(run_id.clone());
            (
                audits
                    .release_slot(codex_app_server_protocol::AuditReleaseSlotParams {
                        run_id,
                        expected_revision: current.revision,
                        reason,
                        acknowledge_possible_concurrent_execution_and_charges: true,
                    })
                    .await?,
                false,
            )
        }
        None => {
            let reviewers = command
                .reviewers
                .iter()
                .map(|value| parse_reviewer(value))
                .collect::<anyhow::Result<Vec<_>>>()?;
            let (model, provider) = if !reviewers.is_empty()
                || (command.model.is_some() && command.provider.is_some())
            {
                (command.model, command.provider)
            } else {
                let response: ConfigReadResponse = requests
                    .request_typed(ClientRequest::ConfigRead {
                        request_id: id("config"),
                        params: ConfigReadParams {
                            include_layers: false,
                            cwd: None,
                        },
                    })
                    .await?;
                (
                    command.model.or(response.config.model),
                    command.provider.or(response.config.model_provider),
                )
            };
            let request_key = command
                .request_key
                .unwrap_or_else(|| uuid::Uuid::now_v7().to_string());
            ensure!(
                !request_key.trim().is_empty(),
                "--request-key must not be empty"
            );
            let mut params = AuditStartParams {
                preflight: None,
                request_key: request_key.clone(),
                source: source.context("missing audit source")?,
                model: if reviewers.is_empty() {
                    Some(model.context("select --model or configure a model on the daemon")?)
                } else {
                    None
                },
                provider: if reviewers.is_empty() {
                    Some(
                        provider
                            .context("select --provider or configure a provider on the daemon")?,
                    )
                } else {
                    None
                },
                reviewers: if reviewers.is_empty() {
                    None
                } else {
                    Some(reviewers)
                },
                scheduling: if command.sequential {
                    codex_app_server_protocol::AuditScheduling::Sequential
                } else {
                    codex_app_server_protocol::AuditScheduling::Automatic
                },
                expected_served_model: command.expected_served_model,
                brief: command.brief,
                reasoning_effort: None,
                audex_guidance,
                agents_evidence,
            };
            eprint!(
                "{}",
                codex_app_server_client::audit::start_disclosure(&params)
            );
            let preflight = audits.preflight(params.clone()).await?;
            eprint!(
                "{}",
                codex_app_server_client::audit::preflight_binding_disclosure(&preflight)
            );
            params.preflight = Some(preflight);
            eprintln!("Audit request key: {request_key}");
            let run = audits.start(params).await.inspect_err(|error| {
                if error.start_acknowledgement_uncertain() {
                    eprintln!("Start acknowledgement is uncertain. Use `audex audit list` or repeat the exact input with --request-key {request_key}; do not submit a new key automatically.");
                }
                if let Some(id) = error.retained_run_id() {
                    eprintln!("A durable run already exists: audex audit read {id} --follow");
                    *retained_run = Some(id.to_owned());
                }
            })?;
            *retained_run = Some(run.run_id.clone());
            if command.detach {
                println!("{}", run.run_id);
                eprintln!(
                    "Submitted durably. Retrieve later: audex audit read {} --follow",
                    run.run_id
                );
                // Success here acknowledges durable submission, not a review verdict.
                return Ok(0);
            }
            (run, true)
        }
    };
    follow_run(client, view, follow).await
}

fn terminal_code(view: &AuditRunView) -> Option<i32> {
    // Published aggregate outcomes take priority over individual execution certainty.
    if view.report.is_some() {
        match view.outcome.as_deref() {
            Some("aborted_by_operator") => return Some(6),
            Some("no_completed_review") => return Some(3),
            _ => {}
        }
    }
    match view.phase {
        AuditRunPhase::Completed => Some(0),
        AuditRunPhase::Cancelled => Some(3),
        AuditRunPhase::Failed | AuditRunPhase::CompletedWithFailures => {
            Some(if view.operation_state == AuditOperationState::Unknown {
                4
            } else {
                2
            })
        }
        _ => None,
    }
}

async fn follow_run(
    client: &mut RemoteAppServerClient,
    mut view: AuditRunView,
    follow: bool,
) -> anyhow::Result<i32> {
    let audits = AuditClient::new(codex_app_server_client::AppServerRequestHandle::Remote(
        client.request_handle(),
    ));
    let mut shown = None;
    let mut next_poll = tokio::time::Instant::now();
    loop {
        if shown.as_ref() != Some(&view) {
            if view.capacity_released {
                eprintln!(
                    "{}: capacity released by operator; this does not establish execution outcome",
                    view.run_id
                );
            }
            eprintln!(
                "{}: {} / {} (revision {}){}{}",
                view.run_id,
                phase_name(&view.phase),
                operation_name(&view.operation_state),
                view.revision,
                if view.cancellation_requested {
                    "; cancellation requested"
                } else {
                    ""
                },
                if view.observation_pending {
                    "; observation pending, no automatic inference retry"
                } else {
                    ""
                }
            );
            if view.collection.reviewer_count > 1 {
                eprintln!(
                    "Collection: {} completed, {} excluded, {} outstanding; ready={}",
                    view.collection.completed_count,
                    view.collection.excluded_count,
                    view.collection.outstanding_count,
                    view.collection.ready
                );
            }
            shown = Some(view.clone());
        }
        if let Some(code) = terminal_code(&view) {
            if let Some(report) = &view.report {
                write_report(report).await?;
            } else if code == 0 {
                bail!("completed audit has no report")
            }
            if code != 0 {
                eprintln!(
                    "Audit outcome: {}",
                    view.outcome.as_deref().unwrap_or("no terminal explanation")
                );
            }
            return Ok(code);
        }
        if !follow {
            return Ok(PENDING);
        }
        let refresh = tokio::select! {
            _ = tokio::time::sleep_until(next_poll) => true,
            event = client.next_event() => match event {
                Some(AppServerEvent::ServerNotification(notification)) => matches!(*notification,
                    ServerNotification::AuditProgress(progress) if progress.run_id == view.run_id && progress.revision > view.revision),
                Some(AppServerEvent::Lagged {..}) => true,
                Some(AppServerEvent::ServerRequest(request)) => {
                    client.reject_server_request(request.id().clone(), JSONRPCErrorError {
                        code: -32601, message: "Audit CLI does not handle interactive server requests".to_string(), data: None,
                    }).await?;
                    false
                }
                Some(AppServerEvent::Disconnected {message}) => bail!("app-server disconnected: {message}"),
                None => bail!("app-server connection closed"),
            }
        };
        if refresh {
            view = audits.read(view.run_id.clone()).await?;
            // Notifications wake immediately; pending fallback reads are slower.
            // Keep an absolute deadline so unrelated events cannot postpone it.
            next_poll = tokio::time::Instant::now()
                + Duration::from_secs(if view.observation_pending { 5 } else { 1 });
        }
    }
}

async fn write_report(report: &str) -> std::io::Result<()> {
    // Flush the handle that owns the queued write before a nonzero process exit.
    let mut stdout = tokio::io::stdout();
    stdout.write_all(report.as_bytes()).await?;
    stdout.flush().await
}

fn phase_name(phase: &AuditRunPhase) -> &'static str {
    match phase {
        AuditRunPhase::Created => "created",
        AuditRunPhase::SnapshotBound => "snapshot_bound",
        AuditRunPhase::Reviewing => "reviewing",
        AuditRunPhase::Reporting => "reporting",
        AuditRunPhase::Reconciling => "reconciling",
        AuditRunPhase::Completed => "completed",
        AuditRunPhase::CompletedWithFailures => "completed_with_failures",
        AuditRunPhase::Failed => "failed",
        AuditRunPhase::Cancelled => "cancelled",
    }
}

fn operation_name(state: &AuditOperationState) -> &'static str {
    match state {
        AuditOperationState::Planned => "planned",
        AuditOperationState::Dispatching => "dispatching",
        AuditOperationState::Accepted => "accepted",
        AuditOperationState::ObservedTerminal => "observed_terminal",
        AuditOperationState::Committed => "committed",
        AuditOperationState::Failed => "failed",
        AuditOperationState::Unknown => "unknown",
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod report_output_tests;
