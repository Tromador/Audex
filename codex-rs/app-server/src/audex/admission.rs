//! Native executor binding for the store's transport-independent admission authority.
use super::capture::CaptureHandle;
use super::store::AuditStore;
use super::store::admission::AdmissionIdentity;
use codex_core::{AdmissionFuture, GenerationAdmission, GenerationAdmissionControl};
use std::sync::Arc;
use std::time::Duration;

pub(super) struct NativeAdmission {
    store: AuditStore,
    identity: AdmissionIdentity,
    capture: CaptureHandle,
    #[cfg(test)]
    checkpoint:
        Option<Arc<dyn Fn(&'static str) -> futures::future::BoxFuture<'static, ()> + Send + Sync>>,
}

impl std::fmt::Debug for NativeAdmission {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fmt.debug_struct("NativeAdmission")
            .field("identity", &self.identity)
            .finish_non_exhaustive()
    }
}

impl NativeAdmission {
    pub(super) fn new(
        store: AuditStore,
        identity: AdmissionIdentity,
        capture: CaptureHandle,
        #[cfg(test)] checkpoint: Option<
            Arc<dyn Fn(&'static str) -> futures::future::BoxFuture<'static, ()> + Send + Sync>,
        >,
    ) -> GenerationAdmissionControl {
        let authority = Arc::new(Self {
            store,
            identity,
            capture,
            #[cfg(test)]
            checkpoint,
        });
        let weak = Arc::downgrade(&authority);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval_at(
                tokio::time::Instant::now() + Duration::from_secs(5),
                Duration::from_secs(5),
            );
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                let Some(authority) = weak.upgrade() else {
                    break;
                };
                if !matches!(
                    authority
                        .store
                        .renew_execution_lease(
                            &authority.identity,
                            chrono::Utc::now().timestamp_millis()
                        )
                        .await,
                    Ok(true)
                ) {
                    authority.capture.request_interrupt();
                    break;
                }
            }
        });
        GenerationAdmissionControl(authority)
    }

    #[cfg(test)]
    async fn checkpoint(&self, name: &'static str) {
        if let Some(checkpoint) = &self.checkpoint {
            checkpoint(name).await;
        }
    }
}

impl GenerationAdmission for NativeAdmission {
    fn admit<'a>(&'a self, request_id: &'a str, transport: &'a str) -> AdmissionFuture<'a, ()> {
        Box::pin(async move {
            #[cfg(test)]
            self.checkpoint("before_generation_admission").await;
            self.store
                .admit_generation(
                    &self.identity,
                    request_id,
                    transport,
                    chrono::Utc::now().timestamp_millis(),
                )
                .await
                .map_err(|_| "durable generation admission refused".to_owned())?;
            #[cfg(test)]
            self.checkpoint("generation_admitted").await;
            Ok(())
        })
    }
    fn may_send<'a>(&'a self, request_id: &'a str) -> AdmissionFuture<'a, bool> {
        Box::pin(async move {
            self.store
                .mark_generation_send(
                    &self.identity,
                    request_id,
                    chrono::Utc::now().timestamp_millis(),
                )
                .await
                .map_err(|_| "generation send fence could not be checked".to_owned())
        })
    }
    fn confirm<'a>(
        &'a self,
        request_id: &'a str,
        response_id: Option<&'a str>,
    ) -> AdmissionFuture<'a, ()> {
        Box::pin(async move {
            self.store
                .confirm_generation(
                    &self.identity,
                    request_id,
                    response_id,
                    chrono::Utc::now().timestamp_millis(),
                )
                .await
                .map_err(|_| "generation receipt could not be retained".to_owned())
        })
    }
}
