//! Retained route selection only. Scheduling and workflow remain in AuditController.
use super::controller::{Dispatch, DispatchRequest, Observation};
use super::store::AuditStore;
use codex_app_server_protocol::AuditExecutionAdapter;
use codex_utils_absolute_path::AbsolutePathBuf;
use futures::future::BoxFuture;
use std::sync::Arc;
use tokio::sync::OnceCell;

pub(super) struct RoutedDispatch {
    root: AbsolutePathBuf,
    native: Arc<dyn Dispatch>,
    store: OnceCell<AuditStore>,
    #[cfg(target_os = "linux")]
    pub external: super::external::ExternalDispatch,
}

impl RoutedDispatch {
    pub fn new(
        root: AbsolutePathBuf,
        native: Arc<dyn Dispatch>,
        bridge: std::path::PathBuf,
    ) -> Self {
        #[cfg(not(target_os = "linux"))]
        let _ = bridge;
        Self {
            root,
            native,
            store: OnceCell::new(),
            #[cfg(target_os = "linux")]
            external: super::external::ExternalDispatch::new(bridge),
        }
    }

    async fn store(&self) -> anyhow::Result<&AuditStore> {
        Ok(self
            .store
            .get_or_try_init(|| AuditStore::open(self.root.clone()))
            .await?)
    }

    async fn external(&self, operation: &str) -> anyhow::Result<bool> {
        let store = self.store().await?;
        if store.client_launch(operation).await?.is_some() {
            return Ok(true);
        }
        let (input, _, _, _) = store.attempt_input(operation).await?;
        Ok(super::retained::RetainedInput::decode(&input)?.adapter()
            == AuditExecutionAdapter::ClaudeClient)
    }

    async fn observe(&self, operation: String, interrupt: bool) -> Observation {
        match self.external(&operation).await {
            Ok(false) => {
                if interrupt {
                    self.native.interrupt(operation).await
                } else {
                    self.native.inspect(operation).await
                }
            }
            #[cfg(target_os = "linux")]
            Ok(true) => {
                if interrupt {
                    self.external.stop(&operation);
                }
                match self.store().await {
                    Ok(store) => self.external.inspect(store, &operation).await,
                    Err(_) => Observation::Deferred("external_store_unavailable".into()),
                }
            }
            _ => Observation::Deferred("retained_dispatch_unavailable".into()),
        }
    }
}

impl Dispatch for RoutedDispatch {
    fn capture(&self, operation: &str) -> Option<super::capture::CaptureHandle> {
        self.native.capture(operation)
    }
    fn start(&self, request: DispatchRequest) -> BoxFuture<'_, Observation> {
        Box::pin(async move {
            match self.external(&request.operation_id).await {
                Ok(false) => self.native.start(request).await,
                #[cfg(target_os = "linux")]
                Ok(true) => match self.store().await {
                    Ok(store) => self.external.start(store, request).await,
                    Err(_) => Observation::Deferred("external_store_unavailable".into()),
                },
                _ => Observation::Unsubmitted("retained_dispatch_unavailable".into()),
            }
        })
    }
    fn inspect(&self, operation: String) -> BoxFuture<'_, Observation> {
        Box::pin(self.observe(operation, false))
    }
    fn interrupt(&self, operation: String) -> BoxFuture<'_, Observation> {
        Box::pin(self.observe(operation, true))
    }
    fn request_interrupt(&self, operation: String) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            match self.external(&operation).await {
                Ok(false) => self.native.request_interrupt(operation).await,
                #[cfg(target_os = "linux")]
                Ok(true) => self.external.stop(&operation),
                _ => {}
            }
        })
    }
    fn cleanup_preparation(&self, operation: String) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            match self.external(&operation).await {
                Ok(false) => self.native.cleanup_preparation(operation).await,
                #[cfg(target_os = "linux")]
                Ok(true) => self.external.stop(&operation),
                _ => {}
            }
        })
    }
}
