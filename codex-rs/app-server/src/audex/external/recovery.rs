//! Called by the existing controller recovery tick, including for terminal audits.
use super::lifecycle::{OwnershipEvidence, same_process};
use super::*;

impl ExternalDispatch {
    pub async fn stop_owned(&self, store: &AuditStore, operation: &str) -> anyhow::Result<()> {
        self.stop(operation);
        let Some(launch) = store.client_launch(operation).await? else {
            return Ok(());
        };
        store
            .fence_client_launch(operation, &launch.execution_id, true)
            .await?;
        // Also reaches a child held in launch preparation, before the collector map exists.
        owner::stop(&launch.execution_id);
        Ok(())
    }

    pub async fn reconcile(&self, store: &AuditStore) -> anyhow::Result<()> {
        use futures::StreamExt;
        let operations = store.unfinished_client_launches().await?;
        let results = futures::stream::iter(operations)
            .map(|operation| async move { self.reconcile_one(store, &operation).await })
            .buffer_unordered(8)
            .collect::<Vec<_>>()
            .await;
        for result in results {
            result?;
        }
        Ok(())
    }

    async fn reconcile_one(&self, store: &AuditStore, operation: &str) -> anyhow::Result<()> {
        let Some(launch) = store.client_launch(operation).await? else {
            return Ok(());
        };
        let state = store.launch_lifecycle(operation).await?;
        if state.cleanup_finished {
            return Ok(());
        }
        let mut evidence: OwnershipEvidence = serde_json::from_str(&state.ownership)?;
        if let Some(owned) = owner::evidence(&launch.execution_id).await {
            for process in owned.processes {
                if !evidence
                    .processes
                    .iter()
                    .any(|p| same_process(&p.identity, &process.identity))
                {
                    evidence.processes.push(process);
                }
            }
            for path in owned.unresolved_socket_paths {
                if !evidence.unresolved_socket_paths.contains(&path) {
                    evidence.unresolved_socket_paths.push(path);
                }
            }
            for socket in owned.sockets {
                if !evidence.sockets.iter().any(|s| s.path == socket.path) {
                    evidence.sockets.push(socket);
                }
            }
        }
        let expected = launch.child_identity().or_else(|| {
            evidence
                .processes
                .iter()
                .find(|p| p.role == "client")
                .map(|p| p.identity.clone())
        });
        let daemon: Option<codex_app_server_daemon::LinuxChildIdentity> = state
            .daemon
            .as_deref()
            .map(serde_json::from_str)
            .transpose()?;
        let daemon_dead = if let Some(daemon) = daemon {
            matches!(
                codex_app_server_daemon::read_linux_process_identity(daemon.pid),
                Ok(None)
            ) || matches!(codex_app_server_daemon::read_linux_process_identity(daemon.pid),
                Ok(Some(actual)) if !same_process(&actual, &daemon))
        } else {
            false
        };
        let directory = PathBuf::from(&launch.mcp_config_path)
            .parent()
            .expect("launch runtime")
            .to_path_buf();
        let nonce = launch.mcp_config_path.clone();
        let binding = state.authority.unwrap_or_default();
        let observe_dir = directory.clone();
        let mut observation = tokio::task::spawn_blocking(move || {
            lifecycle::observe(expected.as_ref(), &nonce, &binding, &observe_dir, &evidence)
        })
        .await?;
        let local_collector = self
            .children
            .lock()
            .expect("client owners")
            .contains_key(operation);
        // A live original daemon may still be between intent and spawn. Its lease
        // remains the boundary; absence alone never authorises replay or early fencing.
        let lost_owner = daemon_dead
            || (launch.prompt_write_started_at_ms.is_some()
                && observation.live.is_empty()
                && !observation.uncertain
                && !local_collector);
        let stop = store
            .fence_client_launch(operation, &launch.execution_id, lost_owner)
            .await?;
        if stop {
            owner::stop(&launch.execution_id);
            self.stop(operation);
            // Cold survivors are signalled individually via verified pidfds.
            for process in &observation.live {
                if lifecycle::signal_verified(&process.identity).is_err() {
                    observation.uncertain = true;
                }
            }
        }
        let stopped = stop && observation.live.is_empty() && !observation.uncertain;
        // Persist attribution before attempting removal, including a crash between these writes.
        store
            .record_local_cleanup(
                operation,
                &launch.execution_id,
                &serde_json::to_string(&observation.evidence)?,
                stopped,
                false,
                observation.reason,
            )
            .await?;
        if stopped {
            let mut evidence = observation.evidence;
            let complete = tokio::task::spawn_blocking(move || {
                let complete = lifecycle::clean_sockets(&mut evidence, &directory).unwrap_or(false);
                (evidence, complete)
            })
            .await?;
            store
                .record_local_cleanup(
                    operation,
                    &launch.execution_id,
                    &serde_json::to_string(&complete.0)?,
                    true,
                    complete.1,
                    if complete.1 {
                        "local_cleanup_confirmed"
                    } else {
                        "socket_cleanup_unconfirmed"
                    },
                )
                .await?;
            if complete.1 {
                owner::forget(&launch.execution_id);
            }
        }
        Ok(())
    }
}
