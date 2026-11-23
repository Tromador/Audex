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
                    claude: service::claude::ClaudeReviewer::new(settings.clone()),
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
                && matches!(tool.callable_name.as_str(), "get_file_info" | "_get_file_info")
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
        if detail.lines().any(|line| line.trim() == "isDirectory: true") {
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
                        if paths.iter().any(|path| service::targets::is_zen_storage_path(path)) {
                            let mut remote_directories = Vec::with_capacity(paths.len());
                            for path in &paths {
                                let kind = if service::targets::is_zen_storage_path(path) {
                                    Some(self.check_zen_storage_path(
                                        path.to_str().context("Zen Storage path is not UTF-8")?,
                                        params.origin_thread_id.as_deref(),
                                    ).await?)
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
                let reviewers = params
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
                let id = service.start(service::StartRequest {
                    input,
                    brief,
                    guidance,
                    reviewers,
                })?;
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

#[cfg(target_os = "linux")]
pub(crate) async fn claude_conversation(
    root: &Path,
    run: &Path,
    model: &str,
    discussion: serde_json::Value,
    message: &str,
) -> Result<String> {
    let settings = BackendSettings::load(root)?;
    let reviewer = service::claude::ClaudeReviewer::new(settings);
    let selection = ReviewerSelection {
        label: "claude".into(),
        route: "claude_subscription".into(),
        model: model.into(),
        effort: None,
        permission_summary: String::new(),
    };
    let parent = root.join("conversations");
    std::fs::create_dir_all(&parent)?;
    let evidence = tempfile::tempdir_in(parent)?;
    let cancellation = tokio_util::sync::CancellationToken::new();
    let outcome = reviewer
        .converse(
            &selection,
            run,
            evidence.path(),
            discussion,
            message,
            &cancellation,
        )
        .await?;
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
