//! HTTP fallback must retain response-scoped model evidence, never generated fields.
use super::*;
use serde_json::json;

async fn attributed(values: Vec<Value>) -> Vec<Result<ResponseEvent, ApiError>> {
    let data = values
        .into_iter()
        .map(|v| format!("data: {v}\n\n"))
        .collect::<String>();
    let stream: ByteStream = Box::pin(futures::stream::iter(vec![Ok(data.into_bytes().into())]));
    let (tx, mut rx) = mpsc::channel(32);
    process_sse_with_treatment(
        stream,
        tx,
        Duration::from_secs(1),
        None,
        SafetyBufferingTreatment::default(),
        true,
    )
    .await;
    let mut events = Vec::new();
    while let Some(event) = rx.recv().await {
        events.push(event);
    }
    events
}

#[tokio::test]
async fn http_lifecycle_binds_creation_before_model_and_model_before_completion() {
    for initial in [None, Some("selected")] {
        let events = attributed(vec![
            json!({"type":"response.created","response":{"id":"one","model":initial}}),
            json!({"type":"response.completed","response":{"id":"one","model":"selected"}}),
        ])
        .await;
        assert!(
            matches!(&events[0], Ok(ResponseEvent::Created { response_id: Some(id) }) if id == "one")
        );
        assert!(matches!(&events[1], Ok(ResponseEvent::ServerModel(model)) if model == "selected"));
        assert!(
            matches!(&events[2], Ok(ResponseEvent::Completed { response_id, .. }) if response_id == "one")
        );
        assert_eq!(events.len(), 3);
    }
}

#[tokio::test]
async fn http_lifecycle_rejects_missing_reused_or_changed_identity() {
    for values in [
        vec![json!({"type":"response.completed","response":{"id":"one","model":"selected"}})],
        vec![json!({"type":"response.created","response":{"model":"selected"}})],
        vec![
            json!({"type":"response.created","response":{"id":"one"}}),
            json!({"type":"response.completed","response":{"id":"two","model":"selected"}}),
        ],
        vec![
            json!({"type":"response.created","response":{"id":"one"}}),
            json!({"type":"response.created","response":{"id":"one","model":"selected"}}),
        ],
    ] {
        let events = attributed(values).await;
        assert!(matches!(
            events.last(),
            Some(Err(ApiError::Transport(
                codex_client::TransportError::GenerationDenied(_)
            )))
        ));
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, Ok(ResponseEvent::ServerModel(_))))
        );
    }
}

#[tokio::test]
async fn http_lifecycle_preserves_conflicts_and_ignores_output_model_fields() {
    let events = attributed(vec![
        json!({"type":"response.created","response":{"id":"one","model":"selected","headers":{"openai-model":"different"}}}),
        json!({"type":"response.output_text.delta","delta":"hello","response":{"id":"one","model":"generated"}}),
        json!({"type":"response.completed","response":{"id":"one","model":"selected"}}),
    ]).await;
    let models = events
        .iter()
        .filter_map(|e| match e {
            Ok(ResponseEvent::ServerModel(model)) => Some(model.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(models, ["different", "selected"]);
}

#[tokio::test]
async fn http_header_only_streams_keep_their_existing_attribution_contract() {
    let events = attributed(vec![
        json!({"type":"codex.response.metadata","headers":{"openai-model":"selected"}}),
        json!({"type":"response.completed","response":{"id":"one"}}),
    ])
    .await;
    assert!(matches!(&events[0], Ok(ResponseEvent::ServerModel(model)) if model == "selected"));
    assert!(matches!(&events[1], Ok(ResponseEvent::Completed { .. })));
}
