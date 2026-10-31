use super::snapshot::{AccessLimits, SearchRequest, SnapshotAccess, SnapshotHandle, SnapshotPath};
use base64::Engine;
use codex_extension_api::{
    ExtensionData, ToolCall, ToolContributor, ToolExecutor, ToolExecutorFuture, ToolName, ToolSpec,
};
use codex_tools::{
    FunctionCallError, JsonToolOutput, ResponsesApiTool, ToolExposure, ToolOutput,
    parse_tool_input_schema,
};
use codex_tools::{ResponsesApiNamespace, ResponsesApiNamespaceTool};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::{Arc, OnceLock};

pub(super) const TOOL_NAMES: [&str; 3] = ["list", "read", "search"];

pub(super) struct SourceTools {
    tools: Vec<Arc<dyn for<'call> ToolExecutor<ToolCall<'call>>>>,
    capture: Arc<OnceLock<super::capture::CaptureHandle>>,
}

impl SourceTools {
    pub(super) fn new(snapshot: &SnapshotHandle) -> anyhow::Result<Self> {
        let mut tools: Vec<Arc<dyn for<'call> ToolExecutor<ToolCall<'call>>>> = Vec::new();
        let capture = Arc::new(OnceLock::new());
        for name in TOOL_NAMES {
            let properties = match name {
                "list" => json!({}),
                "read" => {
                    json!({"path":{"type":"string"}, "offset":{"type":"integer","minimum":0}, "max_bytes":{"type":"integer","minimum":1,"maximum":65536}})
                }
                _ => {
                    json!({"query":{"type":"string","minLength":1,"maxLength":1024}, "path":{"type":["string","null"]}, "max_results":{"type":"integer","minimum":1,"maximum":100}})
                }
            };
            let required = match name {
                "list" => vec![],
                "read" => vec!["path", "offset", "max_bytes"],
                _ => vec!["query", "path", "max_results"],
            };
            let schema = json!({"type":"object","properties":properties,"required":required,"additionalProperties":false});
            let parameters =
                parse_tool_input_schema(&schema).map_err(|error| anyhow::anyhow!("{error}"))?;
            let function = ResponsesApiTool {
                name: name.to_string(),
                description: format!(
                    "{name} immutable manifest-scoped audit source. Byte offsets are zero based."
                ),
                strict: true,
                parameters,
                output_schema: None,
                defer_loading: None,
            };
            tools.push(Arc::new(SourceTool {
                name,
                capture: capture.clone(),
                access: snapshot.access(AccessLimits::default()),
                spec: ToolSpec::Namespace(ResponsesApiNamespace {
                    name: "audex_source".to_string(),
                    description: "Read-only access to the assigned immutable source snapshot."
                        .to_string(),
                    tools: vec![ResponsesApiNamespaceTool::Function(function)],
                }),
            }));
        }
        Ok(Self { tools, capture })
    }
}

impl ToolContributor for SourceTools {
    fn tools(
        &self,
        _: &ExtensionData,
        thread: &ExtensionData,
    ) -> Vec<Arc<dyn for<'call> ToolExecutor<ToolCall<'call>>>> {
        if let Some(capture) = thread.get::<super::capture::CaptureHandle>() {
            let _ = self.capture.set((*capture).clone());
        }
        self.tools.clone()
    }
}

struct SourceTool {
    name: &'static str,
    capture: Arc<OnceLock<super::capture::CaptureHandle>>,
    access: SnapshotAccess,
    spec: ToolSpec,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListArgs {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadArgs {
    path: SnapshotPath,
    offset: u64,
    max_bytes: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchArgs {
    query: String,
    path: Option<SnapshotPath>,
    max_results: usize,
}

impl SourceTool {
    async fn execute(&self, arguments: &str) -> Result<Value, FunctionCallError> {
        // Bound JSON parsing as well as the underlying source operation.
        if arguments.len() > 16 * 1024 {
            return Err(FunctionCallError::RespondToModel(
                "source tool arguments exceed 16 KiB".to_string(),
            ));
        }
        let model_error = |error: super::snapshot::SnapshotError| {
            // Filesystem errors can contain storage paths. Never disclose those to the model.
            let message = match error {
                super::snapshot::SnapshotError::InvalidPath(_) => "invalid manifest path",
                super::snapshot::SnapshotError::EntryNotFound(_) => {
                    "path is not in the snapshot manifest"
                }
                super::snapshot::SnapshotError::AccessLimit(_) => "source access limit exceeded",
                _ => "snapshot integrity or access verification failed",
            };
            FunctionCallError::RespondToModel(message.to_string())
        };
        match self.name {
            "list" => {
                let _: ListArgs = parse(arguments)?;
                Ok(json!({"entries":self.access.list().await.map_err(model_error)?}))
            }
            "read" => {
                let args: ReadArgs = parse(arguments)?;
                let result = self
                    .access
                    .read(&args.path, args.offset, args.max_bytes)
                    .await
                    .map_err(model_error)?;
                let (encoding, content) = match String::from_utf8(result.bytes.clone()) {
                    Ok(text) => ("utf8", text),
                    Err(_) => (
                        "base64",
                        base64::engine::general_purpose::STANDARD.encode(&result.bytes),
                    ),
                };
                Ok(
                    json!({"path":result.path,"sha256":result.sha256,"offset":result.offset,
                    "end_offset":result.offset + result.bytes.len() as u64,
                    "eof":result.eof,"encoding":encoding,"content":content}),
                )
            }
            _ => {
                let args: SearchArgs = parse(arguments)?;
                let hits = self
                    .access
                    .search(SearchRequest {
                        query: args.query.into_bytes(),
                        path: args.path,
                        max_results: args.max_results,
                    })
                    .await
                    .map_err(model_error)?;
                Ok(
                    json!({"hits":hits.into_iter().map(|hit| json!({"path":hit.path,"sha256":hit.sha256,"offset":hit.offset})).collect::<Vec<_>>()}),
                )
            }
        }
    }
}

fn parse<T: serde::de::DeserializeOwned>(arguments: &str) -> Result<T, FunctionCallError> {
    serde_json::from_str(arguments)
        .map_err(|_| FunctionCallError::RespondToModel("invalid source tool arguments".to_string()))
}

impl<'call> ToolExecutor<ToolCall<'call>> for SourceTool {
    fn tool_name(&self) -> ToolName {
        ToolName::namespaced("audex_source", self.name)
    }
    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }
    fn exposure(&self) -> ToolExposure {
        ToolExposure::DirectModelOnly
    }
    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }
    fn handle<'a>(&'a self, call: ToolCall<'call>) -> ToolExecutorFuture<'a>
    where
        'call: 'a,
    {
        Box::pin(async move {
            let arguments = call.function_arguments()?;
            if let Some(capture) = self.capture.get() {
                if !capture.allowed().await {
                    return Err(FunctionCallError::RespondToModel(
                        "reviewer attempt is stopped".to_owned(),
                    ));
                }
                capture
                    .tool(
                        &call.turn_id,
                        &call.call_id,
                        "tool_start",
                        json!({"tool":self.name}).to_string(),
                    )
                    .await
                    .map_err(|_| {
                        FunctionCallError::RespondToModel("activity capture unavailable".to_owned())
                    })?;
            }
            let value = self.execute(arguments).await?;
            if let Some(capture) = self.capture.get() {
                if !capture.allowed().await {
                    return Err(FunctionCallError::RespondToModel(
                        "reviewer attempt is stopped".to_owned(),
                    ));
                }
                capture
                    .tool(
                        &call.turn_id,
                        &call.call_id,
                        "tool_result",
                        value.to_string(),
                    )
                    .await
                    .map_err(|_| {
                        FunctionCallError::RespondToModel("activity capture unavailable".to_owned())
                    })?;
            }
            Ok(Box::new(JsonToolOutput::new(value).with_external_context()) as Box<dyn ToolOutput>)
        })
    }
}
