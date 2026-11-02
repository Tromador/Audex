//! The Audex controller owns reviewer runtimes and history. Client RPCs do not.
use super::*;
use codex_app_server_protocol::{ClientRequest, ClientRequestSerializationScope};

impl ThreadRequestProcessor {
    pub(crate) async fn ensure_audex_client_access(
        &self,
        request: &ClientRequest,
    ) -> Result<(), JSONRPCErrorError> {
        let reject = || {
            invalid_request(
                "Audex reviewer threads are private; use audit/read or audit/cancel for the owning run",
            )
        };
        if let ClientRequest::ThreadAttachmentOwnerList { params, .. } = request
            && params.attachment_type == "audex.review.v1"
        {
            return Err(reject());
        }
        if let ClientRequest::ThreadStart { params, .. } = request
            && matches!(params.thread_source.as_ref(),Some(codex_app_server_protocol::ThreadSource::Feature(name)) if name=="audex-reviewer")
        {
            return Err(reject());
        }
        // Path-based resume/fork can override the supplied ID. Check the actual
        // stored owner through the same resolver used by those operations.
        match request {
            ClientRequest::ThreadResume { params, .. } if params.path.is_some() => {
                let stored = self
                    .read_stored_thread_for_resume(&params.thread_id, params.path.as_ref(), false)
                    .await?;
                if crate::audex::controller::is_reviewer_source(stored.thread_source.as_ref()) {
                    return Err(reject());
                }
            }
            ClientRequest::ThreadFork { params, .. } if params.path.is_some() => {
                let stored = self
                    .read_stored_thread_for_resume(&params.thread_id, params.path.as_ref(), false)
                    .await?;
                if crate::audex::controller::is_reviewer_source(stored.thread_source.as_ref()) {
                    return Err(reject());
                }
            }
            _ => {}
        }
        let target = match request.serialization_scope() {
            Some(ClientRequestSerializationScope::Thread { thread_id }) => Some(thread_id),
            _ => match request {
                ClientRequest::ThreadSample { params, .. } => Some(params.thread_id.clone()),
                ClientRequest::ThreadAttachmentList { params, .. } => {
                    Some(params.thread_id.clone())
                }
                ClientRequest::ThreadSearchOccurrences { params, .. } => {
                    Some(params.thread_id.clone())
                }
                ClientRequest::ThreadTurnsList { params, .. } => Some(params.thread_id.clone()),
                ClientRequest::ThreadItemsList { params, .. } => Some(params.thread_id.clone()),
                ClientRequest::ThreadItemsRead { params, .. } => Some(params.thread_id.clone()),
                _ => None,
            },
        };
        if let Some(id) = target
            && let Ok(id) = ThreadId::from_string(&id)
        {
            if let Ok(thread) = self.thread_manager.get_thread(id).await {
                if crate::audex::controller::is_reviewer_source(
                    thread.config_snapshot().await.thread_source.as_ref(),
                ) {
                    return Err(reject());
                }
            } else {
                match self
                    .thread_store
                    .read_thread(StoreReadThreadParams {
                        thread_id: id,
                        include_archived: true,
                        include_history: false,
                    })
                    .await
                {
                    Ok(stored) if stored.thread_id != id => {
                        return Err(internal_error(format!(
                            "failed to verify thread ownership: store returned thread {}, expected {id}",
                            stored.thread_id
                        )));
                    }
                    Ok(stored)
                        if crate::audex::controller::is_reviewer_source(
                            stored.thread_source.as_ref(),
                        ) =>
                    {
                        return Err(reject());
                    }
                    Ok(_) => {}
                    Err(error) => ownership_lookup_error(id, error)?,
                }
            }
        }
        Ok(())
    }
}

// Only proven absence may reach the ordinary handler, which owns each method's
// missing-thread response (including the empty attachment capability probe).
fn ownership_lookup_error(
    thread_id: ThreadId,
    error: ThreadStoreError,
) -> Result<(), JSONRPCErrorError> {
    match error {
        ThreadStoreError::ThreadNotFound {
            thread_id: missing_id,
        } if missing_id == thread_id => Ok(()),
        ThreadStoreError::InvalidRequest { ref message }
            if message == &format!("no rollout found for thread id {thread_id}") =>
        {
            Ok(())
        }
        // Keep the established stale-session read contract while refusing access.
        error => Err(thread_store_resume_read_error(error)),
    }
}

#[cfg(test)]
#[path = "audex_access_tests.rs"]
mod tests;
