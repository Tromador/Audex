use codex_protocol::models::AudioReference;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ImageReference;
use codex_protocol::models::ResponseInputItem;
use pretty_assertions::assert_eq;

use super::response_input_to_code_mode_result;

#[test]
fn code_mode_text_preserves_message_content() {
    let response = ResponseInputItem::Message {
        role: "user".to_string(),
        content: vec![
            ContentItem::InputText {
                text: "Input text".to_string(),
            },
            ContentItem::OutputText {
                text: "Output text".to_string(),
                annotations: Some(vec![serde_json::json!({"type": "citation"})]),
                logprobs: Some(vec![serde_json::json!({"token": "Output"})]),
            },
            ContentItem::InputImage {
                image: ImageReference::File {
                    file_id: "file_123".to_string(),
                },
                detail: None,
            },
            ContentItem::InputImage {
                image: ImageReference::Inline {
                    image_url: "data:image/png;base64,abc".to_string(),
                },
                detail: None,
            },
            ContentItem::InputAudio {
                audio: AudioReference::Inline {
                    audio_url: "data:audio/wav;base64,def".to_string(),
                },
            },
            ContentItem::InputAudio {
                audio: AudioReference::File {
                    file_id: "file_audio".to_string(),
                },
            },
        ],
        phase: None,
    };

    assert_eq!(
        response_input_to_code_mode_result(response),
        serde_json::json!(
            "Input text\nOutput text\nfile_123\ndata:image/png;base64,abc\ndata:audio/wav;base64,def\nfile_audio"
        )
    );
}

#[test]
fn complete_json_output_preserves_encoded_bytes_and_default_output_policy() {
    use crate::{JsonToolOutput, ToolOutput, ToolPayload};
    use codex_utils_output_truncation::{TruncationPolicy, truncate_function_output_payload};
    let value = serde_json::json!({"content":"\0😀\"\\".repeat(16384)});
    let payload = ToolPayload::Function {
        arguments: "{}".into(),
    };
    let ordinary = JsonToolOutput::new(value.clone()).with_external_context();
    assert_eq!(ordinary.fallback_token_limit_override(), None);
    let complete = ordinary.clone().with_full_output();
    assert_eq!(complete.log_output(), ordinary.log_output());
    assert_eq!(complete.code_mode_result(&payload), value);
    assert!(complete.contains_external_context());
    let ResponseInputItem::FunctionCallOutput { mut output, .. } =
        complete.to_response_item("call", &payload)
    else {
        panic!("function result")
    };
    truncate_function_output_payload(
        &mut output,
        TruncationPolicy::Tokens(complete.fallback_token_limit_override().unwrap()),
        |_| 0,
    );
    assert_eq!(output.body.to_text().unwrap(), value.to_string());
    assert_eq!(
        complete.post_tool_use_response("call", &payload),
        Some(value)
    );
}
