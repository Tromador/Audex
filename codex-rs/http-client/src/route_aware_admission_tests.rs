//! Actual HTTP sends, including a barrier inside asynchronous route preparation.
use super::*;
use crate::{AdmissionFuture, GenerationAdmission, GenerationAdmissionControl, HttpClientBuilder};
use pretty_assertions::assert_eq;
use std::sync::atomic::AtomicBool;
use tokio::sync::Notify;

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
fn guarded(authority: Arc<Authority>) -> RouteAwareClientPool {
    let request = GenerationAdmissionControl(authority).request("logical-generation".to_owned());
    RouteAwareClientPool::with_builder(
        HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
        ClientRouteClass::Api,
        HttpClientBuilder::new().with_generation_admission(request),
    )
    .with_tls_backend_fallback()
}

#[tokio::test]
async fn stop_during_http_route_preparation_prevents_the_physical_send() {
    let authority = Arc::new(Authority::default());
    let pool = guarded(authority.clone());
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let request = reqwest::Request::new(
        http::Method::POST,
        format!("http://{}/responses", listener.local_addr().unwrap())
            .parse()
            .unwrap(),
    );
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let (signal, gate) = (entered.clone(), release.clone());
    let task = tokio::spawn(async move {
        pool.send_with_resolver(request, move |_| {
            let (signal, gate) = (signal.clone(), gate.clone());
            async move {
                signal.notify_one();
                gate.notified().await;
                Ok(OutboundProxyRoute::Direct)
            }
        })
        .await
    });
    entered.notified().await;
    assert_eq!(authority.admissions.load(Ordering::SeqCst), 0);
    authority.stopped.store(true, Ordering::SeqCst);
    release.notify_one();
    assert!(matches!(
        task.await.unwrap(),
        Err(RouteAwareRequestError::GenerationDenied(_))
    ));
    assert_eq!(authority.admissions.load(Ordering::SeqCst), 0);
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
}

#[tokio::test]
async fn admitted_request_stopped_before_wire_is_retained_but_not_sent() {
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let authority = Arc::new(Authority {
        after_admission: Some((entered.clone(), release.clone())),
        ..Default::default()
    });
    let pool = guarded(authority.clone());
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let request = reqwest::Request::new(
        http::Method::POST,
        format!("http://{}/responses", listener.local_addr().unwrap())
            .parse()
            .unwrap(),
    );
    let task = tokio::spawn(async move {
        pool.send_with_resolver(request, |_| async { Ok(OutboundProxyRoute::Direct) })
            .await
    });
    entered.notified().await;
    assert_eq!(authority.admissions.load(Ordering::SeqCst), 1);
    authority.stopped.store(true, Ordering::SeqCst);
    release.notify_one();
    assert!(matches!(
        task.await.unwrap(),
        Err(RouteAwareRequestError::GenerationDenied(_))
    ));
    assert_eq!(authority.admissions.load(Ordering::SeqCst), 1);
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
}

#[tokio::test]
async fn http_retry_rechecks_stop_without_following_redirects() {
    let authority = Arc::new(Authority::default());
    let pool = guarded(authority.clone());
    let (address,server)=spawn_response_server(vec!["HTTP/1.1 307 Temporary Redirect\r\nLocation: /again\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned()]);
    let url = format!("http://{address}/responses");
    let first = reqwest::Request::new(http::Method::POST, url.parse().unwrap());
    let response = pool
        .send_with_resolver(first, |_| async { Ok(OutboundProxyRoute::Direct) })
        .await
        .unwrap();
    assert_eq!(response.status(), http::StatusCode::TEMPORARY_REDIRECT);
    authority.stopped.store(true, Ordering::SeqCst);
    let second = reqwest::Request::new(http::Method::POST, url.parse().unwrap());
    assert!(matches!(
        pool.send_with_resolver(second, |_| async { Ok(OutboundProxyRoute::Direct) })
            .await,
        Err(RouteAwareRequestError::GenerationDenied(_))
    ));
    assert_eq!(server.join().unwrap().len(), 1);
    assert_eq!(authority.admissions.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn guarded_http_tls_failure_uses_normal_transport_fallback() {
    let (url, attempts, stop) =
        super::super::tls_fallback_tests::spawn_protocol_version_rejection_server(2)
            .expect("TLS fixture");
    let authority = Arc::new(Authority::default());
    let pool = guarded(authority.clone());
    super::super::tls_fallback_tests::use_native_tls_for_direct_route(&pool);
    let mut request = reqwest::Request::new(http::Method::POST, url.parse().unwrap());
    *request.body_mut() = Some(bytes::Bytes::from_static(b"replayable generation body").into());
    assert!(
        pool.send_with_resolver(request, |_| async { Ok(OutboundProxyRoute::Direct) })
            .await
            .is_err()
    );
    let _ = stop.send(());
    assert_eq!(
        attempts
            .recv()
            .expect("server finish")
            .expect("TLS attempts"),
        2
    );
    assert_eq!(authority.admissions.load(Ordering::SeqCst), 1);
}
