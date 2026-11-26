//! Thin RPC binding for the single file-backed supervisor.
use super::backends::BackendSettings;
use super::service::{self, AuditService, Reviewer, ReviewerSelection};
use anyhow::{Context, Result, ensure};
use codex_app_server_protocol::*;
use codex_core::{ThreadManager, config::Config};
use futures::future::BoxFuture;
use std::path::Path;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::Mutex;

struct Routes {
    native: service::native::NativeReviewer,
    #[cfg(target_os = "linux")]
    claude: service::claude::ClaudeReviewer,
}
impl Reviewer for Routes {
    fn validate(&self, selection: &ReviewerSelection) -> Result<()> {
        match selection.route.as_str() {
            "native" => self.native.validate(selection),
            #[cfg(target_os = "linux")]
            "claude_subscription" | "claude_api" => self.claude.validate(selection),
            _ => anyhow::bail!("unknown reviewer route: {}", selection.route),
        }
    }
    fn run<'a>(
        &'a self,
        attempt: &'a mut service::Attempt,
    ) -> BoxFuture<'a, Result<service::Outcome>> {
        match attempt.status().reviewer.route.as_str() {
            "native" => self.native.run(attempt),
            #[cfg(target_os = "linux")]
            "claude_subscription" | "claude_api" => self.claude.run(attempt),
            _ => Box::pin(async { anyhow::bail!("unknown reviewer route") }),
        }
    }
}

pub(crate) struct AuditRpc {
    service: Mutex<Result<AuditService>>,
    settings: BackendSettings,
    threads: Option<Arc<ThreadManager>>,
    stopped: AtomicBool,
}
impl AuditRpc {
    pub(crate) fn new(
        config: Arc<Config>,
        threads: Arc<ThreadManager>,
        listen: super::dispatch::Listen,
        cleanup: super::dispatch::Listen,
        outgoing: Arc<crate::outgoing_message::OutgoingMessageSender>,
    ) -> Arc<Self> {
        let root = config.codex_home.join("audex");
        let settings = BackendSettings::load(&root);
        let events_root = root.join("runs");
        let events: service::Events = Arc::new(move |event| {
            let notification = progress(&events_root, event);
            outgoing.try_send_server_notification(
                crate::outgoing_message::NotificationTarget::Broadcast,
                ServerNotification::AuditProgress(notification),
            );
        });
        let (service, settings) = match settings {
            Ok(settings) => {
                let runner = Arc::new(Routes {
                    native: service::native::NativeReviewer::new(
                        config.clone(),
                        threads.clone(),
                        listen,
                        cleanup,
                    ),
                    #[cfg(target_os = "linux")]
                    claude: service::claude::ClaudeReviewer::new(settings.clone(), &root),
                });
                (
                    AuditService::open(&config.codex_home, runner, events),
                    settings,
                )
            }
            Err(error) => (Err(error), BackendSettings::default()),
        };
        Arc::new(Self {
            service: Mutex::new(service),
            settings,
            threads: Some(threads),
            stopped: AtomicBool::new(false),
        })
    }

    /// Reject remote targets before a run or reviewer exists. The originating
    /// Codex thread owns the already-authenticated hosted Apps connection.
    async fn check_zen_storage_path(&self, path: &str, origin: Option<&str>) -> Result<bool> {
        let origin = origin.context("Zen Storage audit requires an originating Codex thread")?;
        let thread_id = codex_protocol::ThreadId::from_string(origin)?;
        let thread = self
            .threads
            .as_ref()
            .context("Zen Storage preflight requires the originating thread manager")?
            .get_thread(thread_id)
            .await
            .context("originating Codex thread is unavailable for Zen Storage preflight")?;
        // Codex Apps has distinct model-visible and raw MCP tool names.
        // Resolve the raw name from this thread's live catalogue, rather than
        // guessing a connector prefix.
        let catalogue = thread
            .refresh_codex_apps_tools()
            .await
            .context("could not discover Zen Storage tools")?;
        let mut matching = catalogue.tools.iter().filter(|tool| {
            tool.callable_namespace.ends_with("__zen_storage")
                && matches!(
                    tool.callable_name.as_str(),
                    "get_file_info" | "_get_file_info"
                )
        });
        let raw_name = matching
            .next()
            .context("Zen Storage get_file_info is unavailable in the Codex Apps catalogue")?
            .tool
            .name
            .as_ref();
        ensure!(
            matching.next().is_none(),
            "Zen Storage get_file_info is ambiguous in the Codex Apps catalogue"
        );
        let response = thread
            .call_mcp_tool(
                "codex_apps",
                None,
                raw_name,
                Some(serde_json::json!({"path": path})),
                None,
            )
            .await
            .with_context(|| format!("Zen Storage preflight failed for {path}"))?;
        let value = serde_json::to_value(&response)?;
        let detail = value["content"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|item| item["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n");
        ensure!(
            response.is_error != Some(true),
            "Zen Storage rejected {path}: {detail}"
        );
        if detail
            .lines()
            .any(|line| line.trim() == "isDirectory: true")
        {
            Ok(true)
        } else if detail.lines().any(|line| line.trim() == "isFile: true") {
            Ok(false)
        } else {
            anyhow::bail!("Zen Storage returned no file/directory type for {path}: {detail}")
        }
    }

    pub(crate) fn stop_new_dispatch(&self) {
        self.stopped.store(true, Ordering::SeqCst);
    }
    pub(crate) async fn quiesce(&self) {
        self.stop_new_dispatch();
        let mut service = self.service.lock().await;
        // Dropping the sole supervisor stops its owned tasks. Restart marks their
        // unfinished records interrupted; it never replays them.
        *service = Err(anyhow::anyhow!("Audex is shutting down"));
    }

    pub(crate) async fn handle(
        &self,
        request: ClientRequest,
    ) -> Result<ClientResponsePayload, JSONRPCErrorError> {
        self.handle_inner(request)
            .await
            .map_err(|error| crate::error_code::invalid_request(format!("Audex: {error:#}")))
    }

    async fn handle_inner(&self, request: ClientRequest) -> Result<ClientResponsePayload> {
        let mut guard = self.service.lock().await;
        let service = guard
            .as_mut()
            .map_err(|error| anyhow::anyhow!("{error:#}"))?;
        match request {
            ClientRequest::AuditDelete { params, .. } => {
                let id = uuid::Uuid::parse_str(&params.run_id)?;
                service.delete(id)?;
                Ok(AuditDeleteResponse {
                    run_id: id.to_string(),
                }
                .into())
            }
            ClientRequest::AuditStart { params, .. } => {
                ensure!(
                    !self.stopped.load(Ordering::SeqCst),
                    "Audex is shutting down"
                );
                let cwd = Path::new(&params.cwd);
                ensure!(cwd.is_absolute(), "cwd must be absolute");
                let input = match params.input {
                    AuditInput::Paths { paths } => {
                        let paths = paths
                            .iter()
                            .map(|path| {
                                if service::targets::is_zen_storage_path(Path::new(path)) {
                                    std::path::PathBuf::from(path)
                                } else {
                                    codex_utils_absolute_path::AbsolutePathBuf::resolve_path_against_base(path, cwd).to_path_buf()
                                }
                            })
                            .collect::<Vec<_>>();
                        if paths
                            .iter()
                            .any(|path| service::targets::is_zen_storage_path(path))
                        {
                            let mut remote_directories = Vec::with_capacity(paths.len());
                            for path in &paths {
                                let kind = if service::targets::is_zen_storage_path(path) {
                                    Some(
                                        self.check_zen_storage_path(
                                            path.to_str()
                                                .context("Zen Storage path is not UTF-8")?,
                                            params.origin_thread_id.as_deref(),
                                        )
                                        .await?,
                                    )
                                } else {
                                    None
                                };
                                remote_directories.push(kind);
                            }
                            service::Input::ResolvedTargets(service::targets::resolve_mixed(
                                &paths,
                                &remote_directories,
                            )?)
                        } else {
                            service::Input::Paths(paths)
                        }
                    }
                    AuditInput::Snippet { name, text } => service::Input::Snippet { name, text },
                };
                let guidance = match (params.guidance, params.guidance_text) {
                    (Some(_), Some(_)) => {
                        anyhow::bail!("Supply only one of guidance and guidanceText")
                    }
                    (Some(path), None) => {
                        let path =
                            codex_utils_absolute_path::AbsolutePathBuf::resolve_path_against_base(
                                path, cwd,
                            );
                        Some(std::fs::read_to_string(path)?)
                    }
                    (None, text) => text,
                };
                let brief = params.brief;
                let mut reviewers: Vec<ReviewerSelection> = params
                    .reviewers
                    .into_iter()
                    .enumerate()
                    .map(|(i, r)| ReviewerSelection {
                        label: format!("reviewer-{}", i + 1),
                        route: r.route,
                        model: r.model,
                        effort: r.effort,
                        permission_summary: String::new(),
                    })
                    .collect();
                // A native selection without a model is the originating conversation itself.
                let mut selected = reviewers
                    .iter()
                    .enumerate()
                    .filter(|(_, r)| r.route == "native" && r.model.is_empty())
                    .map(|(index, _)| index);
                let conversation = selected.next();
                ensure!(
                    selected.next().is_none(),
                    "select the originating conversation (native) at most once; use native:MODEL[:EFFORT] for additional native reviewers"
                );
                let conversation = match conversation {
                    Some(index) => {
                        let origin = params
                            .origin_thread_id
                            .clone()
                            .context("native requires an originating Codex conversation")?;
                        let thread = self
                            .threads
                            .as_ref()
                            .context("native requires the originating thread manager")?
                            .get_thread(codex_protocol::ThreadId::from_string(&origin)?)
                            .await
                            .context("the originating Codex conversation is not loaded")?;
                        let settings = thread.config_snapshot().await;
                        ensure!(
                            !settings.ephemeral,
                            "native requires a persistent Codex conversation"
                        );
                        // Record the conversation's effective settings; the attempt
                        // updates them if they change before the review is admitted.
                        reviewers[index].model = settings.model;
                        reviewers[index].effort = settings
                            .reasoning_effort
                            .and_then(|effort| serde_json::to_value(effort).ok())
                            .and_then(|value| value.as_str().map(str::to_owned));
                        Some(service::ConversationReviewer {
                            thread_id: origin,
                            reviewer: index,
                        })
                    }
                    None => None,
                };
                let id = service.start_with_conversation(
                    service::StartRequest {
                        input,
                        brief,
                        guidance,
                        reviewers,
                    },
                    conversation,
                )?;
                Ok(ClientResponsePayload::AuditStart(AuditRunResponse {
                    run: service::read::view(&service.run_directory(id))?,
                }))
            }
            ClientRequest::AuditList { params, .. } => {
                ensure!(params.limit != Some(0), "limit must be positive");
                let root = service
                    .run_directory(uuid::Uuid::nil())
                    .parent()
                    .context("runs root")?
                    .to_path_buf();
                let runs = service::read::list(&root)?;
                let total = runs.len();
                let runs: Vec<_> = runs
                    .into_iter()
                    .skip(params.offset)
                    .take(params.limit.unwrap_or(usize::MAX))
                    .collect();
                let end = params.offset.min(total) + runs.len();
                Ok(AuditListPage {
                    runs,
                    next_offset: (end < total).then_some(end),
                    presets: self.settings.presets.clone(),
                }
                .into())
            }
            ClientRequest::AuditRead { params, .. } => {
                let id = uuid::Uuid::parse_str(&params.run_id)?;
                Ok(service::read::read(&service.run_directory(id), &params)?.into())
            }
            ClientRequest::AuditCancel { params, .. } => {
                let id = uuid::Uuid::parse_str(&params.run_id)?;
                let directory = service.run_directory(id);
                let record = service::read::record(&directory)?;
                let reviewer = params
                    .reviewer
                    .as_deref()
                    .map(|s| service::read::reviewer_index(&record, s))
                    .transpose()?;
                service.cancel(id, reviewer)?;
                Ok(ClientResponsePayload::AuditCancel(AuditRunResponse {
                    run: service::read::view(&directory)?,
                }))
            }
            ClientRequest::AuditRetry { params, .. } => {
                ensure!(
                    !self.stopped.load(Ordering::SeqCst),
                    "Audex is shutting down"
                );
                let id = uuid::Uuid::parse_str(&params.run_id)?;
                let directory = service.run_directory(id);
                let index = service::read::reviewer_index(
                    &service::read::record(&directory)?,
                    &params.reviewer,
                )?;
                service.retry(id, index).await?;
                Ok(ClientResponsePayload::AuditRetry(AuditRunResponse {
                    run: service::read::view(&directory)?,
                }))
            }
            _ => anyhow::bail!("not an audit request"),
        }
    }
}

fn progress(root: &Path, event: service::Event) -> AuditProgress {
    match event {
        service::Event::Progress {
            run,
            reviewer,
            status,
        } => {
            let directory = service::record::reviewer_dir(
                &root.join(run.to_string()),
                reviewer,
                &status.reviewer,
            )
            .join(format!("attempt-{}", status.attempt));
            match service::read::status(&directory, &status) {
                Ok(reviewer) => AuditProgress {
                    run_id: run.to_string(),
                    reviewer: Some(reviewer),
                    complete: false,
                    error: None,
                },
                Err(error) => AuditProgress {
                    run_id: run.to_string(),
                    reviewer: None,
                    complete: false,
                    error: Some(error.to_string()),
                },
            }
        }
        service::Event::Complete { run, .. } => AuditProgress {
            run_id: run.to_string(),
            reviewer: None,
            complete: true,
            error: None,
        },
        service::Event::Error { run, reason, .. } => AuditProgress {
            run_id: run.to_string(),
            reviewer: None,
            complete: false,
            error: Some(reason),
        },
    }
}

/// How `/claude` discusses a retained audit.
#[cfg(target_os = "linux")]
pub(crate) enum AuditDiscussion {
    /// Resume the audit's Claude reviewer in its own recorded session.
    Session {
        selection: ReviewerSelection,
        label: String,
        attempt: std::path::PathBuf,
        session: service::ClaudeSession,
    },
    /// Discuss the retained reports in a fresh invocation that stores no session.
    Reports {
        selection: ReviewerSelection,
        reason: String,
    },
}

#[cfg(target_os = "linux")]
impl AuditDiscussion {
    pub(crate) fn selection(&self) -> &ReviewerSelection {
        match self {
            Self::Session { selection, .. } | Self::Reports { selection, .. } => selection,
        }
    }

    /// Shown with each reply, so continuity is distinguishable from report-based discussion.
    pub(crate) fn attribution(&self) -> String {
        match self {
            Self::Session {
                selection, label, ..
            } => format!(
                "Claude ({}) — {label}, continuing its audit session",
                selection.model
            ),
            Self::Reports { selection, reason } => format!(
                "Claude ({}) — from retained audit reports; {reason}",
                selection.model
            ),
        }
    }
}

/// Selects the audit's Claude reviewer from its records, without guessing
/// between several. An explicit model overrides only the model; the route and
/// effort remain the reviewer's.
#[cfg(target_os = "linux")]
pub(crate) fn audit_discussion(
    root: &Path,
    run_directory: &Path,
    model: Option<String>,
) -> Result<AuditDiscussion> {
    use service::claude::conversation::selected_model;
    let run = service::read::record(run_directory)?;
    let reviewers: Vec<_> = run
        .reviewers
        .iter()
        .enumerate()
        .filter(|(_, reviewer)| {
            matches!(
                reviewer.route.as_str(),
                "claude_subscription" | "claude_api"
            )
        })
        .collect();
    let [(index, reviewer)] = reviewers[..] else {
        // No single reviewer identifies a session: the existing report-based route.
        let reason = if reviewers.is_empty() {
            "this audit has no Claude reviewer".to_owned()
        } else {
            format!(
                "this audit has {} Claude reviewers, so no session was selected",
                reviewers.len()
            )
        };
        return Ok(AuditDiscussion::Reports {
            selection: ReviewerSelection {
                label: "claude".into(),
                route: "claude_subscription".into(),
                model: selected_model(root, model)?,
                effort: None,
                permission_summary: String::new(),
            },
            reason,
        });
    };
    service::record::validate_label(&reviewer.label)?;
    // The latest successful review is the one its report presents.
    let directory = service::record::reviewer_dir(run_directory, index, reviewer);
    let succeeded = service::record::attempts(&directory)?
        .into_iter()
        .rev()
        .find(|(_, status)| status.state == service::State::Succeeded);
    let mut selection = succeeded
        .as_ref()
        .map_or_else(|| reviewer.clone(), |(_, status)| status.reviewer.clone());
    if let Some(model) = model {
        selection.model = selected_model(root, Some(model))?;
    }
    let label = reviewer.label.clone();
    Ok(match succeeded {
        Some((attempt, status)) => match status.claude_session {
            Some(session) => AuditDiscussion::Session {
                selection,
                label,
                attempt,
                session,
            },
            None => AuditDiscussion::Reports {
                selection,
                reason: format!("{label} has no resumable session"),
            },
        },
        None => AuditDiscussion::Reports {
            selection,
            reason: format!("{label} has no successful review to continue"),
        },
    })
}

/// Exclusive use of one reviewer session for a discussion turn, with its
/// per-thread delivery position. Dropping it releases the session. These files
/// are written only after the attempt has finished, so the attempt's own
/// records keep their single writer.
#[cfg(target_os = "linux")]
pub(crate) struct SessionTurn {
    _lock: std::fs::File,
    path: std::path::PathBuf,
    delivery: service::record::Delivery,
}

#[cfg(target_os = "linux")]
impl SessionTurn {
    pub(crate) fn acquire(attempt: &Path) -> Result<Self> {
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(attempt.join("discussion.lock"))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => anyhow::bail!(
                "this Claude reviewer is already answering in another conversation; try again when it has finished"
            ),
            Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
        }
        let path = attempt.join("discussion.json");
        let delivery = match service::record::read_optional(&path)? {
            Some(bytes) => serde_json::from_slice(&bytes).context("parse discussion.json")?,
            None => service::record::Delivery::default(),
        };
        Ok(Self {
            _lock: lock,
            path,
            delivery,
        })
    }

    /// True until the session has received any shared discussion.
    pub(crate) fn first(&self) -> bool {
        self.delivery.threads.is_empty()
    }

    pub(crate) fn position(&self, thread: &str) -> Option<&str> {
        self.delivery.threads.get(thread).map(String::as_str)
    }

    pub(crate) fn delivered(&mut self, thread: String, turn: String) -> Result<()> {
        self.delivery.threads.insert(thread, turn);
        service::record::write_json(&self.path, &self.delivery)
    }
}

#[cfg(target_os = "linux")]
pub(crate) async fn claude_conversation(
    root: &Path,
    run: &Path,
    selection: &ReviewerSelection,
    discussion: serde_json::Value,
    message: &str,
) -> Result<String> {
    let settings = BackendSettings::load(root)?;
    let reviewer = service::claude::ClaudeReviewer::new(settings, root);
    let evidence = conversation_evidence(root)?;
    let cancellation = tokio_util::sync::CancellationToken::new();
    let outcome = reviewer
        .converse(
            selection,
            run,
            evidence.path(),
            discussion,
            message,
            &cancellation,
        )
        .await?;
    reply(outcome)
}

/// One discussion turn in the reviewer's own session. Follow-up evidence stays
/// in a disposable directory, apart from the attempt's original records.
#[cfg(target_os = "linux")]
pub(crate) async fn claude_resume(
    root: &Path,
    run: &Path,
    selection: &ReviewerSelection,
    session: &service::ClaudeSession,
    assignment: serde_json::Value,
    first: bool,
) -> Result<String> {
    let settings = BackendSettings::load(root)?;
    let reviewer = service::claude::ClaudeReviewer::new(settings, root);
    let evidence = conversation_evidence(root)?;
    let cancellation = tokio_util::sync::CancellationToken::new();
    reviewer
        .resume(
            selection,
            run,
            session,
            evidence.path(),
            assignment,
            first,
            &cancellation,
        )
        .await
        .and_then(reply)
        .map_err(|error| {
            anyhow::anyhow!(
                "reviewer session {} did not complete this turn: {error:#}. No new session was started and the request was not replayed",
                session.id
            )
        })
}

#[cfg(target_os = "linux")]
fn conversation_evidence(root: &Path) -> Result<tempfile::TempDir> {
    let parent = root.join("conversations");
    std::fs::create_dir_all(&parent)?;
    Ok(tempfile::tempdir_in(parent)?)
}

#[cfg(target_os = "linux")]
fn reply(outcome: service::Outcome) -> Result<String> {
    match outcome.completion {
        service::Completion::Succeeded => String::from_utf8(outcome.output.unwrap_or_default())
            .context("Claude output was not UTF-8"),
        service::Completion::Failed(reason) | service::Completion::Interrupted(reason) => {
            anyhow::bail!("{reason}")
        }
        service::Completion::Cancelled => anyhow::bail!("Claude conversation cancelled"),
    }
}

#[cfg(test)]
#[path = "rpc_tests.rs"]
mod tests;
