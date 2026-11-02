use super::snapshot::SnapshotHandle;
use super::source::{SourceService, function};
use codex_extension_api::{
    ExtensionData, ToolCall, ToolContributor, ToolExecutor, ToolExecutorFuture, ToolName, ToolSpec,
};
use codex_tools::{FunctionCallError, JsonToolOutput, ToolExposure, ToolOutput};
use codex_tools::{ResponsesApiNamespace, ResponsesApiNamespaceTool};
use serde_json::json;
use std::sync::{Arc, OnceLock};

pub(super) use super::source::TOOL_NAMES;

pub(super) struct SourceTools {
    tools: Vec<Arc<dyn for<'call> ToolExecutor<ToolCall<'call>>>>,
    capture: Arc<OnceLock<super::capture::CaptureHandle>>,
}

impl SourceTools {
    pub(super) fn new(snapshot: &SnapshotHandle) -> anyhow::Result<Self> {
        Self::with_plain_functions(snapshot, false)
    }

    pub(super) fn with_plain_functions(
        snapshot: &SnapshotHandle,
        plain: bool,
    ) -> anyhow::Result<Self> {
        let mut tools: Vec<Arc<dyn for<'call> ToolExecutor<ToolCall<'call>>>> = Vec::new();
        let capture = Arc::new(OnceLock::new());
        for name in TOOL_NAMES {
            let function = function(name)?;
            tools.push(Arc::new(SourceTool {
                name,
                capture: capture.clone(),
                service: SourceService::new(snapshot),
                plain,
                spec: if plain {
                    let mut function = function;
                    function.name = format!("audex_source__{name}");
                    ToolSpec::Function(function)
                } else {
                    ToolSpec::Namespace(ResponsesApiNamespace {
                        name: "audex_source".to_string(),
                        description: "Read-only access to the assigned immutable source snapshot."
                            .to_string(),
                        tools: vec![ResponsesApiNamespaceTool::Function(function)],
                    })
                },
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
    service: SourceService,
    plain: bool,
    spec: ToolSpec,
}

impl<'call> ToolExecutor<ToolCall<'call>> for SourceTool {
    fn tool_name(&self) -> ToolName {
        if self.plain {
            ToolName::plain(format!("audex_source__{}", self.name))
        } else {
            ToolName::namespaced("audex_source", self.name)
        }
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
            let value = self.service.execute(self.name, arguments).await?;
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
