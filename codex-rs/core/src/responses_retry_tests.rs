use super::ResponsesStreamRequest;
use super::ResponsesStreamRetryState;
use super::handle_response_stream_error;
use super::log_retry;
use crate::realtime_history::RealtimeHistoryState;
use crate::session::step_context::StepContext;
use crate::session::tests::make_session_and_context;
use codex_http_client::RetryAfter;
use codex_protocol::error::CodexErr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::Instant;
use tracing_test::internal::MockWriter;

#[tokio::test]
async fn sampling_retry_logs_stream_error_context() {
    let (_session, turn_context) = make_session_and_context().await;
    let buffer: &'static std::sync::Mutex<Vec<u8>> =
        Box::leak(Box::new(std::sync::Mutex::new(Vec::new())));
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .with_writer(MockWriter::new(buffer))
        .finish();
    let _subscriber_guard = tracing::subscriber::set_default(subscriber);

    log_retry(
        ResponsesStreamRequest::Sampling,
        &turn_context,
        &CodexErr::Stream("websocket closed by server before response.completed".to_string()),
        /*retries*/ 2,
        /*max_retries*/ 5,
        Duration::from_secs(1),
    );

    let logs = String::from_utf8(
        buffer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone(),
    )
    .expect("retry log should be valid utf-8");
    assert!(logs.contains("stream disconnected - retrying sampling request"));
    assert!(logs.contains(&format!("turn_id={}", turn_context.sub_id)));
    assert!(logs.contains("retries=2"));
    assert!(logs.contains("max_retries=5"));
    assert!(logs.contains(
        "sampling_error=stream disconnected before completion: websocket closed by server before response.completed"
    ));
}

/// Time spent reporting a retry must count toward the server's original deadline.
#[tokio::test]
#[expect(
    clippy::await_holding_invalid_type,
    reason = "test holds the event-delivery lock to delay notification while virtual time advances"
)]
async fn stream_retry_preserves_deadline_across_delayed_notification() {
    let (mut session, turn_context) = make_session_and_context().await;
    let step_context = StepContext::for_test(Arc::new(turn_context));
    session.realtime_history = Some(Mutex::new(RealtimeHistoryState::default()));
    let mut client_session = session.services.model_client.new_session();
    // The second retry emits a notification even when release builds hide the first.
    let mut retry_state = ResponsesStreamRetryState {
        retries: 1,
        ..Default::default()
    };

    tokio::time::pause();
    let advice = RetryAfter::from_delay(Duration::from_secs(10)).expect("retry deadline");
    tokio::time::advance(Duration::from_secs(4)).await;

    // Hold the event-delivery lock so reporting the error consumes part of the deadline.
    let history_guard = session.realtime_history.as_ref().unwrap().lock().await;
    let retry = handle_response_stream_error(
        &mut retry_state,
        /*max_retries*/ 2,
        CodexErr::InternalServerError.with_retry_after(advice),
        &mut client_session,
        &session,
        &step_context,
        ResponsesStreamRequest::Sampling,
    );
    tokio::pin!(retry);
    assert!(futures::poll!(&mut retry).is_pending());
    tokio::time::advance(Duration::from_secs(4)).await;
    drop(history_guard);

    assert!(futures::poll!(&mut retry).is_pending());
    tokio::time::advance(Duration::from_secs(1)).await;
    assert!(futures::poll!(&mut retry).is_pending());
    retry.await.expect("retry should be allowed");
    // Tokio rounds timer deadlines up to the next millisecond.
    let resumed_at = Instant::now();
    assert!(
        (advice.deadline()..=advice.deadline() + Duration::from_millis(1)).contains(&resumed_at),
        "retry resumed at {resumed_at:?}, expected {advice:?}"
    );
}

#[derive(Debug)]
struct UnusedAdmission;

impl codex_api::GenerationAdmission for UnusedAdmission {
    fn admit<'a>(&'a self, _: &'a str, _: &'a str) -> codex_api::AdmissionFuture<'a, ()> {
        panic!("error handling must not obtain another admission")
    }

    fn may_send<'a>(&'a self, _: &'a str) -> codex_api::AdmissionFuture<'a, bool> {
        panic!("error handling must not send")
    }

    fn confirm<'a>(&'a self, _: &'a str, _: Option<&'a str>) -> codex_api::AdmissionFuture<'a, ()> {
        panic!("error handling must not manufacture a receipt")
    }
}

#[tokio::test]
async fn admitted_errors_preserve_details_without_retry_guidance_or_fallback() {
    use codex_protocol::error::CodexErrorDetails;

    for max_retries in [0, 4] {
        for details in [
            CodexErrorDetails::RateLimitExceeded("fixture limit".to_owned()),
            CodexErrorDetails::ContentFilter,
            CodexErrorDetails::InternalServerError,
        ] {
            let (session, turn_context) = make_session_and_context().await;
            session
                .services
                .thread_extension_data
                .insert(codex_api::GenerationAdmissionControl(Arc::new(
                    UnusedAdmission,
                )));
            let step_context = StepContext::for_test(Arc::new(turn_context));
            let mut client_session = session.services.model_client.new_session();
            let mut retry_state = ResponsesStreamRetryState::default();
            let history = session.clone_history().await;
            let advice = RetryAfter::from_delay(Duration::from_secs(30)).unwrap();
            let error = CodexErr::new(details).with_retry_after(advice);
            let category = error.to_codex_protocol_error();
            let detail = format!("{:?}", error.details());
            let failure = handle_response_stream_error(
                &mut retry_state,
                max_retries,
                error,
                &mut client_session,
                &session,
                &step_context,
                ResponsesStreamRequest::Sampling,
            );
            // A retry delay would be pending; fallback would return Ok.
            let returned = match futures::FutureExt::now_or_never(failure) {
                Some(Err(error)) => error,
                _ => panic!("admitted failure must return immediately without retry or fallback"),
            };
            assert_eq!(returned.to_codex_protocol_error(), category);
            assert_eq!(format!("{:?}", returned.details()), detail);
            assert_eq!(
                returned.retry_after().unwrap().deadline(),
                advice.deadline()
            );
            assert_eq!(retry_state.retries, 0);
            assert_eq!(retry_state.connection_retries, 0);
            assert!(
                session
                    .clone_history()
                    .await
                    .raw_items()
                    .eq(history.raw_items())
            );
            assert!(
                session
                    .services
                    .thread_extension_data
                    .get::<super::ExhaustedResponseRetry>()
                    .is_none()
            );
        }
    }
}
