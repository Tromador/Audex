//! Owned launch and captured terminal evidence; workflow acceptance stays in the controller.
pub(super) mod commissioning;
mod config;
pub(crate) mod conversation;
mod handshake;
mod lifecycle;
mod owner;
mod policy;
mod recovery;
mod socket_identity;
mod stream;
use super::capture::CaptureHandle;
use super::controller::{DispatchRequest, Observation, WorkState};
use super::retained::RetainedInput;
use super::snapshot::{CaptureLimits, SnapshotId, SnapshotStore};
use super::store::activity::CaptureIdentity;
use super::store::{AuditStore, SubmissionBoundary, admission::AdmissionIdentity};
use codex_exec_server::LocalFileSystem;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

pub(super) struct ExternalDispatch {
    bridge: PathBuf,
    children: Mutex<HashMap<String, OwnedAttempt>>,
    wake: std::sync::OnceLock<Arc<tokio::sync::Notify>>,
    #[cfg(test)]
    pub checkpoint: Option<super::dispatch::Checkpoint>,
}

struct OwnedAttempt {
    child: owner::OwnedChild,
    authority: AdmissionIdentity,
    nonce_argument: String,
    capture: CaptureHandle,
    collector: tokio::task::JoinHandle<()>,
}

impl Drop for OwnedAttempt {
    fn drop(&mut self) {
        self.collector.abort();
    }
}

impl ExternalDispatch {
    pub fn new(bridge: PathBuf) -> Self {
        Self {
            bridge,
            children: Mutex::new(HashMap::new()),
            wake: std::sync::OnceLock::new(),
            #[cfg(test)]
            checkpoint: None,
        }
    }

    pub fn set_wake(&self, wake: Arc<tokio::sync::Notify>) {
        let _ = self.wake.set(wake);
    }

    pub fn capture(&self, operation: &str) -> Option<CaptureHandle> {
        self.children
            .lock()
            .expect("client owners")
            .get(operation)
            .map(|attempt| attempt.capture.clone())
    }

    pub async fn start(&self, store: &AuditStore, request: DispatchRequest) -> Observation {
        match self.start_inner(store, &request).await {
            Ok(observation) => observation,
            Err(_) => {
                self.stop(&request.operation_id);
                match store.submission_boundary(&request.operation_id).await {
                    Ok(SubmissionBoundary::Unsubmitted) => {
                        Observation::Unsubmitted("external_launch_failed_before_prompt".into())
                    }
                    _ => Observation::Deferred("external_submission_outcome_unconfirmed".into()),
                }
            }
        }
    }

    async fn start_inner(
        &self,
        store: &AuditStore,
        request: &DispatchRequest,
    ) -> anyhow::Result<Observation> {
        let run = store.run_for_operation(&request.operation_id).await?;
        anyhow::ensure!(
            run.run_id == request.run_id
                && run.assignment_id == request.assignment_id
                && run.snapshot_id == request.snapshot_id,
            "external request mismatch"
        );
        // Reservation, even without launch evidence, is never permission to spawn again.
        if store.execution(&request.operation_id).await?.is_some() {
            return Ok(self.inspect(store, &request.operation_id).await);
        }
        let (raw, policy, policy_digest, _) = store.attempt_input(&request.operation_id).await?;
        let input = RetainedInput::decode(&raw)?;
        anyhow::ensure!(
            super::snapshot::manifest::sha256_hex(policy.as_bytes()) == policy_digest,
            "invalid policy digest"
        );
        let settings = super::backends::BackendSettings::load(store.root_path())?;
        let snapshot = SnapshotStore::new(
            codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(
                store.root_path().join("snapshots"),
            )?,
            Arc::new(LocalFileSystem::unsandboxed()),
            CaptureLimits::default(),
        )
        .open(&SnapshotId::parse(request.snapshot_id.clone())?)
        .await?;
        let identity = AdmissionIdentity {
            run_id: request.run_id.clone(),
            assignment_id: request.assignment_id.clone(),
            operation_id: request.operation_id.clone(),
            execution_id: uuid::Uuid::now_v7().to_string(),
            owner: request.lease_owner.clone(),
            owner_generation: run.owner_generation,
            run_stop_generation: store.run_stop_generation(&request.run_id).await?,
        };
        let mut prepared = config::prepare(
            store.root_path(),
            &self.bridge,
            &input,
            &policy,
            &snapshot,
            &identity,
            &settings,
        )
        .await?;
        if !store
            .bind_execution(
                &request.operation_id,
                &identity.execution_id,
                &policy_digest,
                &request.lease_owner,
                chrono::Utc::now().timestamp_millis(),
            )
            .await?
        {
            return Ok(self.inspect(store, &request.operation_id).await);
        }
        anyhow::ensure!(
            store
                .record_launch_intent(&identity, &prepared.record)
                .await?,
            "launch intent refused"
        );
        #[cfg(test)]
        self.checkpoint("launch_intent").await;
        anyhow::ensure!(
            super::snapshot::manifest::sha256_hex(&std::fs::read(
                &prepared.record.mcp_config_path
            )?) == prepared.mcp_digest,
            "generated MCP configuration changed before spawn"
        );
        anyhow::ensure!(
            store
                .renew_execution_lease(&identity, chrono::Utc::now().timestamp_millis())
                .await?,
            "launch authority stopped before spawn"
        );
        if let Some(policy) = &prepared.policy {
            policy.recheck()?;
        }
        let spawned = owner::launch(prepared.spec).await?;
        #[cfg(test)]
        self.checkpoint("child_spawned").await;
        // The local owner guard kills on dropped/cancelled preparation; the
        // process-lifetime spawning thread remains alive throughout.
        anyhow::ensure!(
            store
                .record_launch_child(&identity, &spawned.owned.identity)
                .await?,
            "child identity refused"
        );
        #[cfg(test)]
        self.checkpoint("child_identity").await;
        let spawned = if let Some(policy) = &mut prepared.policy {
            handshake::qualify(
                spawned,
                policy,
                input.resolved_model.as_deref().expect("decoded model"),
                prepared.billing_source,
                &prepared.record.client_version,
                store,
                &identity,
            )
            .await?
        } else {
            spawned
        };
        #[cfg(test)]
        self.checkpoint("policy_verified").await;
        input.validate_backend_configuration(&super::backends::BackendSettings::load(
            store.root_path(),
        )?)?;
        if let Some(policy) = &prepared.policy {
            policy.verify_before_prompt()?;
        }
        anyhow::ensure!(
            codex_app_server_daemon::read_linux_child_identity(
                spawned.owned.identity.pid,
                &prepared.record.mcp_config_path
            )? == spawned.owned.identity,
            "child changed before prompt"
        );
        anyhow::ensure!(
            super::snapshot::manifest::sha256_hex(&std::fs::read(
                &prepared.record.mcp_config_path
            )?) == prepared.mcp_digest,
            "generated MCP configuration changed before prompt"
        );
        anyhow::ensure!(
            store.begin_client_prompt(&identity).await?,
            "prompt authority refused"
        );
        #[cfg(test)]
        self.checkpoint("prompt_write_started").await;
        let capture = CaptureHandle::new(
            store.clone(),
            CaptureIdentity {
                assignment_id: identity.assignment_id.clone(),
                session_id: identity.execution_id.clone(),
                owner_generation: identity.owner_generation,
            },
            self.wake.get().cloned(),
        )
        .await?;
        capture.bind_external_interrupt(spawned.owned.interrupt());
        owner::write_prompt(spawned.stdin, &prepared.prompt).await?;
        let collector = stream::Collector {
            store: store.clone(),
            capture: capture.clone(),
            identity: identity.clone(),
            route: prepared.record.route.clone(),
            billing_source: prepared.billing_source,
            expected_model: input.expected_model().to_owned(),
            manifest: snapshot.manifest().clone(),
            #[cfg(test)]
            checkpoint: self.checkpoint.clone(),
        };
        let collector = tokio::spawn(collector.collect(spawned.stdout, spawned.stderr));
        self.children.lock().expect("client owners").insert(
            request.operation_id.clone(),
            OwnedAttempt {
                child: spawned.owned,
                authority: identity,
                nonce_argument: prepared.record.mcp_config_path,
                capture,
                collector,
            },
        );
        #[cfg(test)]
        self.checkpoint("prompt_written").await;
        // Pipe delivery is attempted submission, never observed client acceptance.
        Ok(Observation::Deferred(
            "external_prompt_delivery_attempted_acceptance_unobserved".into(),
        ))
    }

    #[cfg(test)]
    async fn checkpoint(&self, name: &'static str) {
        if let Some(checkpoint) = &self.checkpoint {
            checkpoint(name).await;
        }
    }

    pub async fn inspect(&self, store: &AuditStore, operation: &str) -> Observation {
        #[cfg(test)]
        self.checkpoint("before_inspect").await;
        let authority = self
            .children
            .lock()
            .expect("client owners")
            .get(operation)
            .and_then(|owned| {
                codex_app_server_daemon::read_linux_child_identity(
                    owned.child.identity.pid,
                    &owned.nonce_argument,
                )
                .is_ok_and(|actual| actual == owned.child.identity)
                .then(|| owned.authority.clone())
            });
        if let Some(authority) = authority {
            if !matches!(
                store
                    .renew_execution_lease(&authority, chrono::Utc::now().timestamp_millis())
                    .await,
                Ok(true)
            ) {
                self.stop(operation);
            }
        }
        let run = match store.run_for_operation(operation).await {
            Ok(run) => run,
            Err(_) => return Observation::Deferred("external_attempt_unavailable".into()),
        };
        // Observe failure before loading acceptance: the collector can bind the
        // client and fail between reads, before the controller records acceptance.
        let failure = self
            .capture(operation)
            .and_then(|capture| capture.failure().map(str::to_owned));
        let failure = failure.or(store
            .client_capture_failure(&run.assignment_id)
            .await
            .ok()
            .flatten());
        let launch = match store.client_launch(operation).await {
            Ok(Some(launch)) => launch,
            _ => return Observation::Deferred("external_launch_evidence_unavailable".into()),
        };
        #[cfg(test)]
        self.checkpoint("client_launch_observed").await;
        let accepted = |state| Observation::Accepted {
            operation_id: operation.into(),
            snapshot_id: run.snapshot_id.clone(),
            receipt: format!(
                "client:{}:{}",
                launch.execution_id,
                launch.client_session_id.as_deref().unwrap_or("unobserved")
            ),
            state,
        };
        match store.client_terminal_result(operation).await {
            Ok(Some(raw)) => return accepted(WorkState::Completed(raw)),
            Err(
                super::store::AuditStoreError::Database(_) | super::store::AuditStoreError::Io(_),
            ) => {
                // Read unavailability does not establish an evidence contradiction.
                // Reinspect the same execution through the existing observe-only path.
                return Observation::Deferred("external_terminal_evidence_unavailable".into());
            }
            Err(_) => return Observation::Uncertain("external_terminal_evidence_invalid".into()),
            Ok(None) => {}
        }
        if let Some(failure) = failure {
            return if run.operation_state == super::store::OperationState::Accepted
                || launch.client_accepted_at_ms.is_some()
            {
                accepted(WorkState::Failed(failure))
            } else {
                Observation::Uncertain(failure)
            };
        }
        if launch.client_accepted_at_ms.is_some() {
            return accepted(WorkState::Live);
        }
        match store.submission_boundary(operation).await {
            Ok(SubmissionBoundary::Unsubmitted) => {
                self.stop(operation);
                Observation::Unsubmitted("external_launch_interrupted_before_prompt".into())
            }
            _ => Observation::Deferred("external_submission_outcome_unconfirmed_no_replay".into()),
        }
    }

    pub fn stop(&self, operation: &str) {
        self.children
            .lock()
            .expect("client owners")
            .remove(operation);
    }
}
