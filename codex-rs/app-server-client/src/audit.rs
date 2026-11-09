//! Typed audit requests shared by operator surfaces. No dispatch, retries or storage.

use crate::{AppServerRequestHandle, TypedRequestError};
use codex_app_server_protocol::{
    AUDIT_PROTOCOL_VERSION, AuditCancelParams, AuditCancelResponse, AuditListParams,
    AuditListResponse, AuditReadParams, AuditReadResponse, AuditRunView, AuditStartParams,
    AuditStartResponse, ClientRequest, RequestId,
};
use std::fmt;

#[derive(Debug)]
pub enum AuditClientError {
    Request(TypedRequestError),
    Incompatible(u32),
}

impl AuditClientError {
    pub fn retained_run_id(&self) -> Option<&str> {
        match self {
            Self::Request(TypedRequestError::Server { source, .. }) => {
                let data = source.data.as_ref()?;
                (data.get("reason")?.as_str()? == "auditRunRetained")
                    .then(|| data.get("runId")?.as_str())?
            }
            _ => None,
        }
    }

    pub fn start_acknowledgement_uncertain(&self) -> bool {
        match self {
            Self::Request(
                TypedRequestError::Transport { .. } | TypedRequestError::Deserialize { .. },
            ) => true,
            Self::Request(TypedRequestError::Server { source, .. }) => source
                .data
                .as_ref()
                .and_then(|data| data.get("reason"))
                .and_then(serde_json::Value::as_str)
                .is_some_and(|reason| {
                    matches!(reason, "auditRunRetained" | "auditStartOutcomeUnconfirmed")
                }),
            _ => false,
        }
    }
}

impl fmt::Display for AuditClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Request(error) => error.fmt(f),
            Self::Incompatible(version) => write!(
                f,
                "incompatible Audex daemon protocol {version}; this client requires {AUDIT_PROTOCOL_VERSION}. Update the daemon explicitly before submitting work"
            ),
        }
    }
}

impl std::error::Error for AuditClientError {}

impl From<TypedRequestError> for AuditClientError {
    fn from(error: TypedRequestError) -> Self {
        Self::Request(error)
    }
}

pub fn require_audit_protocol_version(version: u32) -> Result<(), AuditClientError> {
    if version != AUDIT_PROTOCOL_VERSION {
        return Err(AuditClientError::Incompatible(version));
    }
    Ok(())
}

/// Shared CLI/TUI disclosure of every selected route, before submission.
/// Availability does not assert that credentials or billing have been verified.
pub fn reviewer_selection_disclosure(params: &AuditStartParams) -> String {
    let roster = params.reviewers.clone().unwrap_or_else(|| {
        vec![codex_app_server_protocol::AuditReviewerSelection {
            provider: params.provider.clone().unwrap_or_default(),
            model: params.model.clone().unwrap_or_default(),
            expected_served_model: params.expected_served_model.clone(),
            reasoning_effort: params.reasoning_effort.clone(),
            execution_adapter: Default::default(),
            authentication_route: Default::default(),
        }]
    });
    let mut text = "Ordered roster (including queued reviewers):\n".to_owned();
    for (index, reviewer) in roster.iter().enumerate() {
        let availability = match reviewer.validate() {
            Err(reason) => reason,
            Ok(()) if reviewer.authentication_route
                == codex_app_server_protocol::AuditAuthenticationRoute::DaemonConfiguredNative =>
                "daemon credentials; authentication/billing category and amounts are not verified here",
            Ok(()) if reviewer.authentication_route
                == codex_app_server_protocol::AuditAuthenticationRoute::ClaudeSubscription =>
                "Official Claude client using OAuth in the dedicated subscription profile; consumes subscription quota. Account eligibility and billing are checked before the prompt, not by this read-only preflight.\n   The client may initiate corrective requests after cut-off output (up to 3), a missing tool call (up to 2), or empty output (up to 1), per occurrence. These are not a total request ceiling or Audex Retry authority.\n   Audex records observed corrections and stops best effort. Already initiated requests and remote quota charges may continue; stopping does not prove prevention. No automatic resume, model substitution or API fallback",
            Ok(()) => "unavailable for commissioning; authentication and billing configuration have not been verified",
        };
        text.push_str(&format!(
            "{}. {} / {}; executor={}; authentication route={}; reasoning={:?}; served-model binding={:?}\n   {availability}.\n",
            index + 1, reviewer.model, reviewer.provider, reviewer.execution_adapter,
            reviewer.authentication_route, reviewer.reasoning_effort, reviewer.expected_served_model
        ));
    }
    text
}

fn context_disclosure(value: Option<&str>) -> String {
    value
        .map(|text| {
            format!(
                "{} UTF-8 bytes; preview: {}",
                text.len(),
                text.chars().take(160).collect::<String>()
            )
        })
        .unwrap_or_else(|| "not supplied".to_owned())
}

pub fn start_disclosure(params: &codex_app_server_protocol::AuditStartParams) -> String {
    let source = match &params.source {
        codex_app_server_protocol::AuditSource::File { name, path } => {
            format!("file {path} (snapshot name {name})")
        }
        codex_app_server_protocol::AuditSource::Snippet { name, content } => format!(
            "snippet {name} ({} UTF-8 bytes); preview: {}",
            content.len(),
            content.chars().take(160).collect::<String>()
        ),
    };
    let mut text = format!(
        "Source: {source}\nBrief: {}\nScheduling: {:?} (automatic is bounded; sequential permits one active reviewer reservation)\n{}",
        params.brief,
        params.scheduling,
        reviewer_selection_disclosure(params)
    );
    text.push_str(&format!("AUDEX guidance: {}\nAGENTS evidence: {}\nIf the complete roster passes preflight, selected source, brief and explicit context will be disclosed to every selected provider, including queued reviewers. An unavailable route refuses the entire roster. All reviewers use the same sealed snapshot. Operator tools and history are not inherited.\nRequest key: {}",context_disclosure(params.audex_guidance.as_deref()),context_disclosure(params.agents_evidence.as_deref()),params.request_key));
    if let Some(preflight) = &params.preflight {
        text.push_str(&preflight_binding_disclosure(preflight));
    }
    text
}

pub fn preflight_binding_disclosure(
    preflight: &codex_app_server_protocol::AuditPreflightResponse,
) -> String {
    let mut text = format!(
        "\nLocal preflight binding: {}\nSource snapshot: {}\nManifest SHA-256: {}\nBrief SHA-256: {}\nContext SHA-256: {}\n",
        preflight.identity,
        preflight.snapshot_id,
        preflight.manifest_digest,
        preflight.brief_digest,
        preflight.context_digest
    );
    for (index, reviewer) in preflight.reviewers.iter().enumerate() {
        text.push_str(&format!("{}. Requested model: {}; resolved: {}; expected served: {}; reasoning: {:?}\n   Backend configuration identity: {}\n",
            index + 1, reviewer.selection.model, reviewer.resolved_model,
            reviewer.expected_served_model, reviewer.reasoning_effort,
            reviewer.backend_configuration_identity));
        if let Some(client) = &reviewer.client {
            text.push_str(&format!("   Official client executable: {}\n   Client version: {}; executable SHA-256: {}\n   Dedicated subscription profile: {}\n   Private runtime root: {}\n   Qualified local policy SHA-256: {}\n",
                client.executable, client.client_version, client.executable_digest,
                client.profile, client.runtime_root, client.policy_digest));
        }
    }
    if let Some(run) = &preflight.existing_run_id {
        let bindings = if preflight.reviewers.is_empty() {
            "Historical reviewer configuration bindings were not recorded. The retained request identity matches."
        } else {
            "These are the retained approval bindings."
        };
        text.push_str(&format!("Existing audit: {run}. {bindings} An identical Start retrieves that audit; it commissions no new work.\n"));
    } else {
        text.push_str("Start rechecks these bindings; drift refuses the entire roster.\n");
    }
    text.push_str(
        "Preflight starts no client, refreshes no credentials and performs no inference.\n",
    );
    text
}

#[derive(Clone)]
pub struct AuditClient {
    handle: AppServerRequestHandle,
}

impl AuditClient {
    pub fn new(handle: AppServerRequestHandle) -> Self {
        Self { handle }
    }

    pub async fn check_compatibility(&self) -> Result<(), AuditClientError> {
        let response: codex_app_server_protocol::AuditCapabilitiesResponse = self
            .handle
            .request_typed(ClientRequest::AuditCapabilities {
                request_id: request_id("capabilities"),
                params: codex_app_server_protocol::AuditCapabilitiesParams {},
            })
            .await?;
        require_audit_protocol_version(response.protocol_version)
    }

    pub async fn preflight(
        &self,
        request: AuditStartParams,
    ) -> Result<codex_app_server_protocol::AuditPreflightResponse, AuditClientError> {
        let response: codex_app_server_protocol::AuditPreflightResponse = self
            .handle
            .request_typed(ClientRequest::AuditPreflight {
                request_id: request_id("preflight"),
                params: codex_app_server_protocol::AuditPreflightParams { request },
            })
            .await?;
        require_audit_protocol_version(response.protocol_version)?;
        Ok(response)
    }

    pub async fn list(
        &self,
        params: AuditListParams,
    ) -> Result<AuditListResponse, AuditClientError> {
        let response: AuditListResponse = self
            .handle
            .request_typed(ClientRequest::AuditList {
                request_id: request_id("list"),
                params,
            })
            .await?;
        require_audit_protocol_version(response.protocol_version)?;
        Ok(response)
    }

    /// Callers check compatibility before offering submission. No automatic retry.
    pub async fn start(&self, params: AuditStartParams) -> Result<AuditRunView, AuditClientError> {
        let response: AuditStartResponse = self
            .handle
            .request_typed(ClientRequest::AuditStart {
                request_id: request_id("start"),
                params,
            })
            .await?;
        Ok(response.run)
    }

    pub async fn evidence(
        &self,
        params: codex_app_server_protocol::AuditEvidenceParams,
    ) -> Result<serde_json::Value, AuditClientError> {
        let response: codex_app_server_protocol::AuditEvidenceResponse = self
            .handle
            .request_typed(ClientRequest::AuditEvidence {
                request_id: request_id("evidence"),
                params,
            })
            .await?;
        Ok(response.data)
    }

    pub async fn read(&self, run_id: String) -> Result<AuditRunView, AuditClientError> {
        let response: AuditReadResponse = self
            .handle
            .request_typed(ClientRequest::AuditRead {
                request_id: request_id("read"),
                params: AuditReadParams { run_id },
            })
            .await?;
        Ok(response.run)
    }

    pub async fn cancel(&self, run_id: String) -> Result<AuditRunView, AuditClientError> {
        let response: AuditCancelResponse = self
            .handle
            .request_typed(ClientRequest::AuditCancel {
                request_id: request_id("cancel"),
                params: AuditCancelParams { run_id },
            })
            .await?;
        Ok(response.run)
    }

    pub async fn attempts(
        &self,
        run_id: String,
    ) -> Result<codex_app_server_protocol::AuditAttemptsResponse, AuditClientError> {
        Ok(self
            .handle
            .request_typed(ClientRequest::AuditAttempts {
                request_id: request_id("attempts"),
                params: codex_app_server_protocol::AuditAttemptsParams { run_id },
            })
            .await?)
    }

    pub async fn read_attempt(
        &self,
        params: codex_app_server_protocol::AuditAttemptReadParams,
    ) -> Result<codex_app_server_protocol::AuditAttemptReadResponse, AuditClientError> {
        Ok(self
            .handle
            .request_typed(ClientRequest::AuditAttemptRead {
                request_id: request_id("attempt-read"),
                params,
            })
            .await?)
    }

    pub async fn control_attempt(
        &self,
        params: codex_app_server_protocol::AuditAttemptControlParams,
    ) -> Result<codex_app_server_protocol::AuditAttemptControlResponse, AuditClientError> {
        Ok(self
            .handle
            .request_typed(ClientRequest::AuditAttemptControl {
                request_id: request_id("attempt-control"),
                params,
            })
            .await?)
    }

    pub async fn release_slot(
        &self,
        params: codex_app_server_protocol::AuditReleaseSlotParams,
    ) -> Result<AuditRunView, AuditClientError> {
        let response: codex_app_server_protocol::AuditReleaseSlotResponse = self
            .handle
            .request_typed(ClientRequest::AuditReleaseSlot {
                request_id: request_id("release-slot"),
                params,
            })
            .await?;
        Ok(response.run)
    }
}

fn request_id(method: &str) -> RequestId {
    // Handles multiplex requests by ID; concurrent TUI observers need distinct IDs.
    static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let id = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    RequestId::String(format!("audex-client-{method}-{id}"))
}

#[cfg(test)]
#[path = "audit_tests.rs"]
mod tests;
