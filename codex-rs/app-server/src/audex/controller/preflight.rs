//! Read-only public commissioning preview. No store opening or process launch.
use super::*;
mod replay;
use crate::audex::backends::BackendSettings;
use crate::audex::retained::native_configuration_identity;
pub(super) use replay::request_identity;

pub(super) fn native_adapter(value: &AuditExecutionAdapter) -> bool {
    *value == AuditExecutionAdapter::Native
}
pub(super) fn native_route(value: &AuditAuthenticationRoute) -> bool {
    *value == AuditAuthenticationRoute::DaemonConfiguredNative
}

pub(super) fn selections(params: &AuditStartParams) -> anyhow::Result<Vec<ReviewerSelection>> {
    let roster = if let Some(roster) = &params.reviewers {
        anyhow::ensure!(
            params.provider.is_none()
                && params.model.is_none()
                && params.reasoning_effort.is_none()
                && params.expected_served_model.is_none(),
            "roster and singular reviewer selection are mutually exclusive"
        );
        roster.clone()
    } else {
        vec![AuditReviewerSelection {
            provider: params
                .provider
                .clone()
                .ok_or_else(|| anyhow::anyhow!("reviewer provider is required"))?,
            model: params
                .model
                .clone()
                .ok_or_else(|| anyhow::anyhow!("reviewer model is required"))?,
            reasoning_effort: params.reasoning_effort.clone(),
            expected_served_model: params.expected_served_model.clone(),
            execution_adapter: Default::default(),
            authentication_route: Default::default(),
        }]
    };
    anyhow::ensure!(
        !roster.is_empty() && roster.len() <= 64,
        "reviewer roster must contain 1..64 entries"
    );
    roster
        .into_iter()
        .map(|selection| {
            selection.validate().map_err(anyhow::Error::msg)?;
            Ok(ReviewerSelection {
                provider: selection.provider,
                model: selection.model,
                reasoning_effort: selection.reasoning_effort,
                expected_served_model: selection.expected_served_model,
                execution_adapter: selection.execution_adapter,
                authentication_route: selection.authentication_route,
            })
        })
        .collect()
}

pub(super) fn capture_request(params: &AuditStartParams) -> anyhow::Result<CaptureRequest> {
    Ok(CaptureRequest {
        source: match &params.source {
            AuditSource::Snippet { name, content } => CaptureSource::Snippet {
                logical_path: SnapshotPath::new(name.clone())?,
                bytes: content.as_bytes().to_vec(),
            },
            AuditSource::File { name, path } => CaptureSource::File {
                logical_path: SnapshotPath::new(name.clone())?,
                source_path: AbsolutePathBuf::from_absolute_path(path)?,
            },
        },
        context_documents: ContextDocuments {
            audex_guidance: params
                .audex_guidance
                .as_ref()
                .map(|s| s.as_bytes().to_vec()),
            agents_evidence: params
                .agents_evidence
                .as_ref()
                .map(|s| s.as_bytes().to_vec()),
        },
    })
}

impl ReviewerSelection {
    fn wire(&self) -> AuditReviewerSelection {
        AuditReviewerSelection {
            provider: self.provider.clone(),
            model: self.model.clone(),
            reasoning_effort: self.reasoning_effort.clone(),
            expected_served_model: self.expected_served_model.clone(),
            execution_adapter: self.execution_adapter.clone(),
            authentication_route: self.authentication_route.clone(),
        }
    }
}

impl AuditController {
    fn validate_native_preflight(
        &self,
        manifest: &super::super::snapshot::SnapshotManifestV1,
    ) -> anyhow::Result<()> {
        use codex_features::Feature;
        use codex_protocol::config_types::WebSearchMode;
        use codex_protocol::models::{ManagedFileSystemPermissions, PermissionProfile};
        use codex_protocol::permissions::{
            FileSystemAccessMode, FileSystemSandboxEntry, NetworkSandboxPolicy,
        };
        use codex_protocol::protocol::AskForApproval;
        let mut config = (*self.config).clone();
        for feature in config.features.enabled_features() {
            config.features.disable(feature)?;
        }
        anyhow::ensure!(
            config
                .features
                .enabled_features()
                .iter()
                .all(|f| *f == Feature::UnifiedExec),
            "managed feature requirements mandate ambient reviewer capabilities"
        );
        config.web_search_mode.set(WebSearchMode::Disabled)?;
        config.mcp_servers.set(Default::default())?;
        config
            .permissions
            .approval_policy
            .set(AskForApproval::Never)?;
        let source = self
            .snapshots()
            .root()
            .join(manifest.snapshot_id.digest())
            .join("source");
        config
            .permissions
            .set_permission_profile(PermissionProfile::Managed {
                file_system: ManagedFileSystemPermissions::Restricted {
                    entries: vec![FileSystemSandboxEntry::new(
                        source.clone().into(),
                        FileSystemAccessMode::Read,
                    )],
                    glob_scan_max_depth: None,
                },
                network: NetworkSandboxPolicy::Restricted,
            })?;
        config.permissions.set_workspace_roots(vec![source.clone()]);
        let fs = config.permissions.file_system_sandbox_policy();
        anyhow::ensure!(
            !fs.has_full_disk_read_access() && !fs.has_full_disk_write_access(),
            "reviewer profile exposes an unintended host root"
        );
        for entry in &manifest.identity.entries {
            let path = entry
                .path
                .components()
                .fold(source.clone(), |root, part| root.join(part));
            anyhow::ensure!(
                fs.can_read_local_path_with_cwd(path.as_path(), source.as_path())
                    && !fs.can_write_local_path_with_cwd(path.as_path(), source.as_path()),
                "managed filesystem policy is incompatible with audited source"
            );
        }
        Ok(())
    }

    pub(super) async fn preflight(
        &self,
        params: &AuditStartParams,
    ) -> anyhow::Result<AuditPreflightResponse> {
        let selections = selections(params)?;
        anyhow::ensure!(
            !params.request_key.trim().is_empty(),
            "request key must be nonempty"
        );
        anyhow::ensure!(
            params.brief.len() <= 64 * 1024,
            "audit brief exceeds 64 KiB"
        );
        for selection in &selections {
            anyhow::ensure!(
                matches!(
                    selection.authentication_route,
                    AuditAuthenticationRoute::DaemonConfiguredNative
                        | AuditAuthenticationRoute::ClaudeSubscription
                ),
                "reviewer route {} is unavailable for commissioning",
                selection.authentication_route
            );
        }
        let manifest = self.snapshots().preview(capture_request(params)?).await?;
        if let Some(retained) = self
            .retained_preflight(params, &selections, &manifest)
            .await?
        {
            return Ok(retained);
        }
        anyhow::ensure!(
            !self.stop.is_cancelled() && self.dispatch.available(),
            "reviewer execution is unavailable"
        );
        let settings = BackendSettings::load(self.root.as_path())?;
        let mut reviewers = Vec::with_capacity(selections.len());
        for selection in selections {
            let (resolved, effort, native, metadata, client) = match selection.authentication_route
            {
                AuditAuthenticationRoute::DaemonConfiguredNative => {
                    reviewer::validate_selection_requirements(
                        &self.config,
                        &selection.provider,
                        &selection.model,
                    )?;
                    let native = native_configuration_identity(&self.config, &selection.provider)?;
                    // Native preparation constructs a default isolated configuration,
                    // not the host's context/compaction or instruction overrides.
                    // Consult the same in-memory catalogue without loading Config.
                    let config = codex_models_manager::ModelsManagerConfig {
                        personality: Some(codex_protocol::config_types::Personality::None),
                        ..Default::default()
                    };
                    let model = self.models.get_model_info(&selection.model, &config).await;
                    let effort = selection
                        .reasoning_effort
                        .as_deref()
                        .map(str::parse)
                        .transpose()
                        .map_err(anyhow::Error::msg)?
                        .or(model.default_reasoning_level.clone());
                    if let Some(effort) = &effort {
                        anyhow::ensure!(
                            model
                                .supported_reasoning_levels
                                .iter()
                                .any(|level| &level.effort == effort),
                            "model does not support the selected reasoning effort"
                        );
                    }
                    (
                        model.slug.clone(),
                        effort.map(|e| e.to_string()),
                        Some(native),
                        Some(sha256_hex(&serde_json::to_vec(model.model.as_ref())?)),
                        None,
                    )
                }
                #[cfg(target_os = "linux")]
                AuditAuthenticationRoute::ClaudeSubscription => {
                    let client = super::super::external::commissioning::inspect(
                        self.root.as_path(),
                        &settings,
                    )?;
                    anyhow::ensure!(
                        selection.provider == "anthropic",
                        "subscription provider must be anthropic"
                    );
                    anyhow::ensure!(
                        selection.reasoning_effort.is_none(),
                        "subscription reasoning effort is not supported"
                    );
                    for model in [
                        Some(&selection.model),
                        selection.expected_served_model.as_ref(),
                    ]
                    .into_iter()
                    .flatten()
                    {
                        anyhow::ensure!(
                            model.starts_with("claude-")
                                && model.len() <= 1024
                                && model.bytes().all(|b| b.is_ascii_alphanumeric()
                                    || matches!(b, b'-' | b'.' | b'_')),
                            "subscription model must be an explicit Claude model identity"
                        );
                    }
                    (selection.model.clone(), None, None, None, Some(client))
                }
                _ => anyhow::bail!(
                    "reviewer route {} is unavailable for commissioning",
                    selection.authentication_route
                ),
            };
            reviewers.push(AuditReviewerBinding {
                selection: selection.wire(),
                expected_served_model: selection
                    .expected_served_model
                    .clone()
                    .unwrap_or_else(|| resolved.clone()),
                resolved_model: resolved,
                reasoning_effort: effort,
                backend_configuration_identity: settings
                    .identity(&selection.authentication_route)?,
                native_configuration_identity: native,
                model_metadata_digest: metadata,
                client,
            });
        }

        if reviewers
            .iter()
            .any(|r| native_route(&r.selection.authentication_route))
        {
            self.validate_native_preflight(&manifest)?;
        }
        replay::response(params, &manifest, reviewers)
    }
}

#[cfg(target_os = "linux")]
pub(super) fn subscription_commission(
    params: &AuditStartParams,
    snapshot: &SnapshotHandle,
    approved: &AuditReviewerBinding,
) -> anyhow::Result<super::super::store::ReviewerCommission> {
    use super::super::retained::{ExecutionLimits, PolicyBindings, RetainedInput};
    let client = approved
        .client
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("missing subscription bindings"))?;
    let policy = super::super::external::commissioning::retained_policy(client)?;
    let policy_json = serde_json::to_string(&policy)?;
    let policy_digest = sha256_hex(policy_json.as_bytes());
    let manifest_digest = sha256_hex(&snapshot.manifest().to_pretty_bytes()?);
    let selection = &approved.selection;
    let input = RetainedInput {
        version: 2,
        brief: params.brief.clone(),
        provider: selection.provider.clone(),
        model: selection.model.clone(),
        reasoning_effort: None,
        require_served_identity: true,
        expected_served_model: selection.expected_served_model.clone(),
        resolved_model: Some(approved.resolved_model.clone()),
        execution_adapter: Some(selection.execution_adapter.clone()),
        authentication_route: Some(selection.authentication_route.clone()),
        bindings: Some(PolicyBindings {
            version: 1,
            snapshot_id: snapshot.snapshot_id().to_string(),
            manifest_digest: manifest_digest.clone(),
            brief_digest: sha256_hex(params.brief.as_bytes()),
            context_digest: sha256_hex(&serde_json::to_vec(
                &snapshot.manifest().identity.context_documents,
            )?),
            policy_digest: policy_digest.clone(),
            backend_configuration_identity: approved.backend_configuration_identity.clone(),
            native_configuration_identity: None,
            scheduling: params.scheduling.clone(),
            tool_exposure: super::super::source::TOOL_NAMES
                .map(|n| format!("mcp__audex_source__{n}"))
                .to_vec(),
            limits: ExecutionLimits::native(),
        }),
    };
    let raw = serde_json::to_string(&input)?;
    RetainedInput::decode(&raw)?;
    let digest = sha256_hex(raw.as_bytes());
    Ok(super::super::store::ReviewerCommission {
        request: CreateRunRequest {
            request_key: params.request_key.clone(),
            snapshot_id: snapshot.snapshot_id().to_string(),
            manifest_digest,
            manifest_path: format!(
                "snapshots/{}/manifest.json",
                snapshot.snapshot_id().digest()
            ),
            brief_digest: sha256_hex(params.brief.as_bytes()),
            policy_digest: policy_digest.clone(),
            input_digest: digest.clone(),
            provider: selection.provider.clone(),
            model: selection.model.clone(),
            reasoning_effort: None,
            reviewer_input_digest: sha256_hex(format!("{digest}:{policy_digest}").as_bytes()),
            operation_input_digest: digest,
        },
        input: Some((raw, policy_json)),
    })
}

#[cfg(not(target_os = "linux"))]
pub(super) fn subscription_commission(
    _params: &AuditStartParams,
    _snapshot: &SnapshotHandle,
    _approved: &AuditReviewerBinding,
) -> anyhow::Result<super::super::store::ReviewerCommission> {
    anyhow::bail!("subscription route is unavailable on this platform")
}
