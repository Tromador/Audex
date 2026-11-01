//! Optional host authority at the physical generation send boundary.
use crate::TransportError;
use std::fmt::Debug;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

pub type AdmissionFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, String>> + Send + 'a>>;

/// Implementations must commit one-use admission before returning success.
/// A receipt is operational evidence; it does not authorise another request.
pub trait GenerationAdmission: Debug + Send + Sync {
    fn admit<'a>(&'a self, request_id: &'a str, transport: &'a str) -> AdmissionFuture<'a, ()>;
    fn may_send<'a>(&'a self, request_id: &'a str) -> AdmissionFuture<'a, bool>;
    fn confirm<'a>(
        &'a self,
        request_id: &'a str,
        response_id: Option<&'a str>,
    ) -> AdmissionFuture<'a, ()>;
}

/// Carried in isolated thread extension state. Ordinary sessions omit it.
#[derive(Debug, Clone)]
pub struct GenerationAdmissionControl(pub Arc<dyn GenerationAdmission>);

#[derive(Debug, Clone)]
pub struct GenerationRequest {
    pub request_id: String,
    control: GenerationAdmissionControl,
    attempted: Arc<AtomicBool>,
}

impl GenerationAdmissionControl {
    pub fn request(&self, request_id: String) -> GenerationRequest {
        GenerationRequest {
            request_id,
            control: self.clone(),
            attempted: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl GenerationRequest {
    pub fn attempted(&self) -> bool {
        self.attempted.load(Ordering::SeqCst)
    }

    /// Invoked only after auth/preparation and, for WS, the writer lock.
    pub async fn admit(&self, transport: &str) -> Result<(), TransportError> {
        if self.attempted.swap(true, Ordering::SeqCst) {
            return Err(TransportError::GenerationDenied(
                "generation admission is one-use".to_owned(),
            ));
        }
        self.control
            .0
            .admit(&self.request_id, transport)
            .await
            .map_err(TransportError::GenerationDenied)?;
        // Suppress a pre-admitted send where the durable stop is already visible.
        // The admission remains irreversible even when this check refuses sending.
        if !self
            .control
            .0
            .may_send(&self.request_id)
            .await
            .map_err(TransportError::GenerationDenied)?
        {
            return Err(TransportError::GenerationDenied(
                "attempt stopped after admission".to_owned(),
            ));
        }
        Ok(())
    }

    pub async fn confirm(&self, response_id: Option<&str>) -> Result<(), TransportError> {
        self.control
            .0
            .confirm(&self.request_id, response_id)
            .await
            .map_err(TransportError::GenerationDenied)
    }
}
