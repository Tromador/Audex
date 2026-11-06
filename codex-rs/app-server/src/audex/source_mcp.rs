//! Attempt-bound stdio MCP transport. It never creates a store or writes an audit record.
use super::snapshot::{CaptureLimits, SnapshotId, SnapshotStore, manifest::sha256_hex};
use super::source::{SourceService, TOOL_NAMES, function};
use super::store::{admission::AdmissionIdentity, source::ReadOnlySource};
use clap::Args;
use codex_exec_server::LocalFileSystem;
use codex_utils_absolute_path::AbsolutePathBuf;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::{ErrorData, ServerHandler, ServiceExt};
use std::path::PathBuf;
use std::sync::Arc;

/// Internal CLI transport configuration. Models cannot set any of these fields.
#[derive(Debug, Args)]
pub struct AudexSourceMcpArgs {
    #[arg(long)]
    pub state_root: PathBuf,
    /// Bounded non-secret AdmissionIdentity JSON; execution_id is the launch nonce.
    #[arg(long)]
    pub binding: String,
    #[arg(long)]
    pub snapshot: String,
    #[arg(long)]
    pub manifest_digest: String,
    #[arg(long)]
    pub parent_pid: u32,
}

pub(super) struct SourceMcpServer {
    service: SourceService,
}

impl SourceMcpServer {
    pub(super) fn new(service: SourceService) -> Self {
        Self { service }
    }
}

impl ServerHandler for SourceMcpServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build()).with_server_info(
            Implementation::new("audex-source", env!("CARGO_PKG_VERSION")),
        )
    }

    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        if request.is_some_and(|r| r.cursor.is_some()) {
            return Err(ErrorData::invalid_params(
                "source tools have no pagination cursor",
                None,
            ));
        }
        let tools = TOOL_NAMES
            .into_iter()
            .map(|name| {
                let spec = function(name)
                    .map_err(|_| ErrorData::internal_error("invalid source schema", None))?;
                let schema = serde_json::to_value(spec.parameters)
                    .and_then(serde_json::from_value)
                    .map_err(|_| ErrorData::internal_error("invalid source schema", None))?;
                let mut meta = rmcp::model::MetaObject::new();
                meta.insert(
                    "anthropic/maxResultSizeChars".into(),
                    serde_json::json!(super::source::MAX_RESULT_BYTES),
                );
                Ok(Tool::new(spec.name, spec.description, Arc::new(schema)).with_meta(meta))
            })
            .collect::<Result<Vec<_>, ErrorData>>()?;
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        if !TOOL_NAMES.contains(&request.name.as_ref()) {
            return Err(ErrorData::invalid_params("unknown source tool", None));
        }
        let args = serde_json::Value::Object(request.arguments.unwrap_or_default()).to_string();
        let result = match self.service.execute(&request.name, &args).await {
            Ok(value) => {
                let mut result =
                    CallToolResult::success(vec![ContentBlock::text(value.to_string())]);
                result.structured_content = Some(value);
                result
            }
            Err(error) => CallToolResult::error(vec![ContentBlock::text(error.to_string())]),
        };
        Ok(result.into())
    }
}

/// Serve the actual bridge over stdin/stdout, with no normal app-server startup.
pub async fn run_audex_source_mcp(args: AudexSourceMcpArgs) -> anyhow::Result<()> {
    // Zero selects the durable launch-parent binding; explicit parents retain
    // the existing standalone bridge qualification interface.
    #[cfg(target_os = "linux")]
    let parent = if args.parent_pid == 0 {
        (unsafe { libc::getppid() }) as u32
    } else {
        args.parent_pid
    };
    #[cfg(not(target_os = "linux"))]
    let parent = args.parent_pid;
    bind_parent(parent)?;
    anyhow::ensure!(args.binding.len() <= 16 * 1024, "invalid source binding");
    let identity: AdmissionIdentity = serde_json::from_str(&args.binding)?;
    let root = AbsolutePathBuf::from_absolute_path(args.state_root)?;
    let id = SnapshotId::parse(args.snapshot)?;
    let authority = ReadOnlySource::open(
        &root,
        identity,
        id.to_string(),
        args.manifest_digest.clone(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("source authority unavailable or refused"))?;
    #[cfg(target_os = "linux")]
    authority
        .verify_launch_parent(parent, args.parent_pid == 0)
        .await?;
    bind_parent(parent)?;
    let snapshot = SnapshotStore::new(
        root.join("snapshots"),
        Arc::new(LocalFileSystem::unsandboxed()),
        CaptureLimits::default(),
    )
    .open(&id)
    .await
    .map_err(|_| anyhow::anyhow!("snapshot verification failed"))?;
    anyhow::ensure!(
        sha256_hex(&snapshot.manifest().to_pretty_bytes()?) == args.manifest_digest,
        "snapshot binding mismatch"
    );
    let service = SourceService::new(&snapshot).with_authority(authority);
    let transport = rmcp::transport::async_rw::AsyncRwTransport::new_server(
        tokio::io::stdin(),
        tokio::io::stdout(),
    )
    .with_max_line_length(64 * 1024);
    let running = SourceMcpServer::new(service).serve(transport).await?;
    running.waiting().await?;
    Ok(())
}

fn bind_parent(expected: u32) -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: prctl changes only this bridge's parent-death signal; no pointers.
        anyhow::ensure!(
            unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) } == 0,
            "cannot protect bridge parent lifetime"
        );
        // The parent may have died before prctl; never serve that orphan.
        anyhow::ensure!(
            (unsafe { libc::getppid() }) as u32 == expected,
            "bridge parent changed"
        );
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = expected;
        anyhow::bail!("Audex source MCP currently requires Linux/WSL")
    }
}
