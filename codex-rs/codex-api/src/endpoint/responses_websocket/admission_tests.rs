//! Wire-level placement of the opt-in gate, including auth and writer-lock waits.
use super::*;
use crate::common::{ResponseCreateWsRequest, ResponsesApiRequest};
use crate::{AdmissionFuture, GenerationAdmission, GenerationAdmissionControl, GenerationRequest};
use crate::{AuthError, AuthHeadersFuture, AuthProvider, ResponsesClient, RetryConfig};
use codex_http_client::{
    ClientRouteClass, HttpClientBuilder, OutboundProxyPolicy, RouteAwareClientPool,
};
use serde_json::json;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tokio::net::TcpListener;
use tokio::sync::Notify;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

#[derive(Debug, Default)]
struct Authority {
    stopped: AtomicBool,
    admissions: AtomicUsize,
    after_admission: Option<(Arc<Notify>, Arc<Notify>)>,
}
impl GenerationAdmission for Authority {
    fn admit<'a>(&'a self, _id: &'a str, _transport: &'a str) -> AdmissionFuture<'a, ()> {
        Box::pin(async move {
            if self.stopped.load(Ordering::SeqCst) {
                return Err("stopped".to_owned());
            }
            self.admissions.fetch_add(1, Ordering::SeqCst);
            if let Some((entered, release)) = &self.after_admission {
                entered.notify_one();
                release.notified().await;
            }
            Ok(())
        })
    }
    fn may_send<'a>(&'a self, _id: &'a str) -> AdmissionFuture<'a, bool> {
        Box::pin(async { Ok(!self.stopped.load(Ordering::SeqCst)) })
    }
    fn confirm<'a>(&'a self, _id: &'a str, _response: Option<&'a str>) -> AdmissionFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }
}
struct NoAuth;
impl AuthProvider for NoAuth {
    fn add_auth_headers(&self, _: &mut HeaderMap) -> Result<(), AuthError> {
        Ok(())
    }
}
struct PausedAuth {
    entered: Arc<Notify>,
    release: Arc<Notify>,
}
impl AuthProvider for PausedAuth {
    fn add_auth_headers(&self, _: &mut HeaderMap) -> Result<(), AuthError> {
        Ok(())
    }
    fn apply_auth_headers(&self, headers: HeaderMap) -> AuthHeadersFuture<'_> {
        Box::pin(async move {
            self.entered.notify_one();
            self.release.notified().await;
            Ok(headers)
        })
    }
}
fn provider(base_url: String) -> Provider {
    Provider {
        name: "local admission fixture".to_owned(),
        base_url,
        query_params: None,
        headers: HeaderMap::new(),
        retry: RetryConfig {
            max_attempts: 4,
            base_delay: Duration::from_millis(1),
            retry_429: true,
            retry_5xx: true,
            retry_transport: true,
        },
        stream_idle_timeout: Duration::from_secs(5),
    }
}
fn request() -> ResponsesApiRequest {
    ResponsesApiRequest {
        model: "fixture".to_owned(),
        stream: true,
        service_tier: None,
        input: Vec::new(),
        tools: None,
        tool_choice: "auto".to_owned(),
        parallel_tool_calls: false,
        reasoning: None,
        store: false,
        stream_options: None,
        include: Vec::new(),
        prompt_cache_key: None,
        text: None,
        client_metadata: None,
        access_programs: None,
    }
}
fn logical(authority: &Arc<Authority>) -> GenerationRequest {
    GenerationAdmissionControl(authority.clone()).request("one-logical-request".to_owned())
}
async fn wait(signal: &Notify) {
    tokio::time::timeout(Duration::from_secs(5), signal.notified())
        .await
        .expect("barrier reached");
}

#[tokio::test]
async fn asynchronous_http_auth_does_not_admit_until_after_auth_and_stop_prevents_send() {
    let server = MockServer::start().await;
    let authority = Arc::new(Authority::default());
    let generation = logical(&authority);
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let factory = HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault);
    let transport = codex_client::ReqwestTransport::from_http_client(
        RouteAwareClientPool::with_builder(
            factory,
            ClientRouteClass::Api,
            HttpClientBuilder::new().with_generation_admission(generation.clone()),
        )
        .into_client(),
    );
    let client = ResponsesClient::new(
        transport,
        provider(server.uri()),
        Arc::new(PausedAuth {
            entered: entered.clone(),
            release: release.clone(),
        }),
    )
    .with_generation_admission(Some(generation));
    let task = tokio::spawn(async move {
        client
            .stream(
                json!({"model":"fixture","input":[],"stream":true}),
                HeaderMap::new(),
                crate::Compression::None,
                None,
            )
            .await
    });
    wait(&entered).await;
    assert_eq!(authority.admissions.load(Ordering::SeqCst), 0);
    assert!(server.received_requests().await.unwrap().is_empty());
    authority.stopped.store(true, Ordering::SeqCst);
    release.notify_one();
    assert!(matches!(
        task.await.unwrap(),
        Err(ApiError::Transport(TransportError::GenerationDenied(_)))
    ));
    assert_eq!(authority.admissions.load(Ordering::SeqCst), 0);
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn guarded_http_failure_sends_once_while_ordinary_http_keeps_its_retry_behaviour() {
    for guarded in [true, false] {
        let server = MockServer::start().await;
        let seen = Arc::new(AtomicUsize::new(0));
        let count = seen.clone();
        Mock::given(method("POST")).and(path("/responses")).respond_with(move |_:&wiremock::Request| {
            if count.fetch_add(1,Ordering::SeqCst)==0 {ResponseTemplate::new(503)} else {
                ResponseTemplate::new(200).insert_header("content-type","text/event-stream").set_body_string("data: {\"type\":\"response.completed\",\"response\":{\"id\":\"done\"}}\n\n")
            }
        }).mount(&server).await;
        let authority = Arc::new(Authority::default());
        let generation = logical(&authority);
        let builder = if guarded {
            HttpClientBuilder::new().with_generation_admission(generation.clone())
        } else {
            HttpClientBuilder::new()
        };
        let transport = codex_client::ReqwestTransport::from_http_client(
            RouteAwareClientPool::with_builder(
                HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
                ClientRouteClass::Api,
                builder,
            )
            .into_client(),
        );
        let client = ResponsesClient::new(transport, provider(server.uri()), Arc::new(NoAuth))
            .with_generation_admission(guarded.then_some(generation));
        let result = client
            .stream(
                json!({"model":"fixture","input":[],"stream":true}),
                HeaderMap::new(),
                crate::Compression::None,
                None,
            )
            .await;
        if guarded {
            assert!(
                matches!(
                    result,
                    Err(ApiError::Transport(TransportError::Http {
                        status: StatusCode::SERVICE_UNAVAILABLE,
                        ..
                    }))
                ),
                "the provider failure must not become a second-admission denial"
            );
        } else {
            assert!(result.is_ok());
        }
        assert_eq!(seen.load(Ordering::SeqCst), if guarded { 1 } else { 2 });
        assert_eq!(
            authority.admissions.load(Ordering::SeqCst),
            usize::from(guarded)
        );
    }
}

async fn connection() -> (
    ResponsesWebsocketConnection,
    Arc<StdMutex<Vec<Value>>>,
    tokio::task::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let frames = Arc::new(StdMutex::new(Vec::new()));
    let observed = frames.clone();
    let task = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut websocket =
            tokio_tungstenite::accept_async_with_config(socket, Some(websocket_config()))
                .await
                .unwrap();
        while let Some(Ok(message)) = websocket.next().await {
            match message {
                Message::Text(text) => {
                    let value: Value = serde_json::from_str(&text).unwrap();
                    observed.lock().unwrap().push(value.clone());
                    if value["type"] == "response.create" {
                        websocket.send(Message::Text(json!({"type":"response.created","response":{"id":"active-response"}}).to_string().into())).await.unwrap();
                    } else if value["type"] == "response.interrupt" {
                        for event in [
                            json!({"type":"response.interrupt.accepted","response_id":"active-response"}),
                            json!({"type":"response.completed","response":{"id":"active-response","output":[]}}),
                        ] {
                            websocket
                                .send(Message::Text(event.to_string().into()))
                                .await
                                .unwrap();
                        }
                    }
                }
                Message::Close(_) => {
                    observed
                        .lock()
                        .unwrap()
                        .push(json!({"type":"socket.close"}));
                    break;
                }
                _ => {}
            }
        }
    });
    let client =
        ResponsesWebsocketClient::new(provider(format!("http://{address}")), Arc::new(NoAuth));
    let connection = client
        .connect(
            &HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
            HeaderMap::new(),
            HeaderMap::new(),
            None,
            None,
        )
        .await
        .unwrap();
    (connection, frames, task)
}

#[tokio::test]
async fn websocket_admission_runs_after_writer_lock_and_stop_refuses_the_create_frame() {
    let (connection, frames, server) = connection().await;
    let authority = Arc::new(Authority::default());
    let generation = logical(&authority);
    let held = connection.stream.lock().await;
    let body = request();
    let mut stream = connection
        .stream_request_with_admission(
            ResponsesWsRequest::ResponseCreate(ResponseCreateWsRequest::from(&body)),
            false,
            None,
            Some(generation),
        )
        .await
        .unwrap();
    tokio::task::yield_now().await;
    assert_eq!(
        authority.admissions.load(Ordering::SeqCst),
        0,
        "the queued writer has no permit"
    );
    authority.stopped.store(true, Ordering::SeqCst);
    drop(held);
    let event = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        event,
        Err(ApiError::Transport(TransportError::GenerationDenied(_)))
    ));
    assert_eq!(authority.admissions.load(Ordering::SeqCst), 0);
    drop(stream);
    drop(connection);
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
    assert!(
        frames
            .lock()
            .unwrap()
            .iter()
            .all(|frame| frame["type"] != "response.create")
    );
}

#[tokio::test]
async fn websocket_interrupt_and_close_frames_remain_available_after_generation_fence_closes() {
    let (connection, frames, server) = connection().await;
    let authority = Arc::new(Authority::default());
    let body = request();
    let mut stream = connection
        .stream_request_with_admission(
            ResponsesWsRequest::ResponseCreate(ResponseCreateWsRequest::from(&body)),
            false,
            None,
            Some(logical(&authority)),
        )
        .await
        .unwrap();
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await
            .unwrap(),
        Some(Ok(ResponseEvent::Created { .. }))
    ));
    authority.stopped.store(true, Ordering::SeqCst);
    assert!(
        stream
            .interrupt
            .take()
            .unwrap()
            .send(ResponseInterrupt::Cancel {
                deadline: tokio::time::Instant::now() + Duration::from_secs(5)
            })
            .is_ok()
    );
    loop {
        let event = tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        if matches!(event, ResponseEvent::Completed { .. }) {
            break;
        }
    }
    {
        let mut held = connection.stream.lock().await;
        held.as_mut()
            .unwrap()
            .send(Message::Close(None))
            .await
            .unwrap();
    }
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
    let kinds = frames
        .lock()
        .unwrap()
        .iter()
        .map(|frame| frame["type"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        kinds,
        vec!["response.create", "response.interrupt", "socket.close"]
    );
    assert_eq!(authority.admissions.load(Ordering::SeqCst), 1);
}
