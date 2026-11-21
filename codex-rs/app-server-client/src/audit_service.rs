//! Typed operator requests. No retries, source inspection or orchestration.
use crate::{AppServerRequestHandle, TypedRequestError};
use codex_app_server_protocol::{
    AuditCancelRequest, AuditInput, AuditListPage, AuditListRequest, AuditReadRequest,
    AuditReadResult, AuditRetryRequest, AuditRun, AuditRunResponse, AuditStartRequest,
    ClientRequest, RequestId,
};

pub struct AuditClient {
    handle: AppServerRequestHandle,
}
impl AuditClient {
    pub fn new(handle: AppServerRequestHandle) -> Self {
        Self { handle }
    }
    pub async fn delete(&self, run_id: String) -> Result<(), TypedRequestError> {
        let _: codex_app_server_protocol::AuditDeleteResponse = self
            .handle
            .request_typed(ClientRequest::AuditDelete {
                request_id: id(),
                params: codex_app_server_protocol::AuditDeleteRequest { run_id },
            })
            .await?;
        Ok(())
    }
    pub async fn start(&self, params: AuditStartRequest) -> Result<AuditRun, TypedRequestError> {
        let response: AuditRunResponse = self
            .handle
            .request_typed(ClientRequest::AuditStart {
                request_id: id(),
                params,
            })
            .await?;
        Ok(response.run)
    }
    pub async fn list(&self, params: AuditListRequest) -> Result<AuditListPage, TypedRequestError> {
        self.handle
            .request_typed(ClientRequest::AuditList {
                request_id: id(),
                params,
            })
            .await
    }
    pub async fn read(
        &self,
        params: AuditReadRequest,
    ) -> Result<AuditReadResult, TypedRequestError> {
        self.handle
            .request_typed(ClientRequest::AuditRead {
                request_id: id(),
                params,
            })
            .await
    }
    pub async fn cancel(&self, params: AuditCancelRequest) -> Result<AuditRun, TypedRequestError> {
        let response: AuditRunResponse = self
            .handle
            .request_typed(ClientRequest::AuditCancel {
                request_id: id(),
                params,
            })
            .await?;
        Ok(response.run)
    }
    pub async fn retry(&self, params: AuditRetryRequest) -> Result<AuditRun, TypedRequestError> {
        let response: AuditRunResponse = self
            .handle
            .request_typed(ClientRequest::AuditRetry {
                request_id: id(),
                params,
            })
            .await?;
        Ok(response.run)
    }
}
fn id() -> RequestId {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    RequestId::String(format!(
        "audit-{}",
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ))
}

pub fn read_run(run_id: String) -> AuditReadRequest {
    AuditReadRequest {
        run_id,
        view: None,
        reviewer: None,
        attempt: None,
        number: None,
        offset: 0,
        limit: None,
    }
}

pub fn start_disclosure(params: &AuditStartRequest) -> String {
    let targets = match &params.input {
        AuditInput::Paths { paths } => paths.join("\n"),
        AuditInput::Snippet { name, .. } => format!("Pasted input: {name}"),
    };
    let mut text = format!("Targets:\n{targets}\n\nReviewers:\n");
    for reviewer in &params.reviewers {
        let disclosure = if reviewer.route == "native" {
            "source is sent to the configured native model provider (external unless configured locally)"
        } else {
            "source is sent to Anthropic (external provider)"
        };
        text.push_str(&format!(
            "{}: {} — effort: {} — {}\n",
            reviewer.route,
            reviewer.model,
            reviewer.effort.as_deref().unwrap_or("default"),
            disclosure
        ));
    }
    if let Some(guidance) = &params.guidance {
        text.push_str(&format!("\nGuidance file: {guidance}\n"));
    }
    if let Some(text_guidance) = &params.guidance_text {
        text.push_str(&format!("\nInline guidance:\n{text_guidance}\n"));
    }
    text.push_str("\nRead-only audit. Completed results enter this conversation; two or more completed reviewers trigger one Codex comparison using the conversation's current model.");
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AppServerClient, CloudClient, CloudOperation};
    use serde_json::json;

    #[tokio::test]
    async fn delete_uses_only_the_selected_run() {
        let (cloud, mut commands, _incoming) = CloudClient::channel(1);
        let client = AuditClient::new(AppServerClient::Cloud(cloud).request_handle());
        let work = client.delete("selected".into());
        let server = async {
            let command = commands.recv().await.unwrap();
            let CloudOperation::Request(request) = command.operation else {
                panic!()
            };
            assert!(
                matches!(*request, ClientRequest::AuditDelete { params, .. } if params.run_id == "selected")
            );
            command
                .response
                .send(Ok(Ok(json!({"runId":"selected"}))))
                .unwrap();
        };
        let (result, ()) = tokio::join!(work, server);
        result.unwrap();
        assert!(commands.try_recv().is_err());
    }

    #[tokio::test]
    async fn five_wrappers_use_only_the_approved_wire_methods() {
        let (cloud, mut commands, _incoming) = CloudClient::channel(8);
        let client = AuditClient::new(AppServerClient::Cloud(cloud).request_handle());
        let run: AuditRun = serde_json::from_value(json!({"runId":"run","targets":["source"],"createdAt":"time","reviewers":[],"complete":false,"summary":null,"sourceNotice":"Source may have changed"})).unwrap();
        let expected = run.clone();
        let requests = async {
            assert_eq!(
                client
                    .start(AuditStartRequest {
                        input: AuditInput::Paths {
                            paths: vec!["source".into()]
                        },
                        cwd: "/cwd".into(),
                        brief: "brief".into(),
                        reviewers: vec![],
                        guidance: None,
                        guidance_text: None,
                    })
                    .await
                    .unwrap(),
                expected
            );
            assert_eq!(
                client
                    .list(AuditListRequest {
                        offset: 2,
                        limit: Some(3)
                    })
                    .await
                    .unwrap()
                    .next_offset,
                Some(5)
            );
            assert_eq!(
                client.read(read_run("run".into())).await.unwrap().run,
                expected
            );
            assert_eq!(
                client
                    .cancel(AuditCancelRequest {
                        run_id: "run".into(),
                        reviewer: Some("2".into())
                    })
                    .await
                    .unwrap(),
                expected
            );
            assert_eq!(
                client
                    .retry(AuditRetryRequest {
                        run_id: "run".into(),
                        reviewer: "1".into()
                    })
                    .await
                    .unwrap(),
                expected
            );
        };
        let server = async {
            for method in [
                "audit/start",
                "audit/list",
                "audit/read",
                "audit/cancel",
                "audit/retry",
            ] {
                let command = commands.recv().await.unwrap();
                let CloudOperation::Request(request) = command.operation else {
                    panic!("typed request")
                };
                assert_eq!(request.method_name(), method);
                let response = match method {
                    "audit/list" => json!({"runs":[run],"nextOffset":5,"presets":{}}),
                    "audit/read" => json!({"run":run,"data":null}),
                    _ => json!({"run":run}),
                };
                command.response.send(Ok(Ok(response))).unwrap();
            }
        };
        tokio::join!(requests, server);
    }
}
