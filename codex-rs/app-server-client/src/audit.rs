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
