//! Transport-independent schemas and execution over the existing sealed snapshot access.
use super::snapshot::{AccessLimits, SearchRequest, SnapshotAccess, SnapshotHandle, SnapshotPath};
use base64::Engine;
use codex_tools::{FunctionCallError, ResponsesApiTool, parse_tool_input_schema};
use futures::future::BoxFuture;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

pub(super) const TOOL_NAMES: [&str; 3] = ["list", "read", "search"];

pub(super) trait SourceAuthority: Send + Sync {
    fn allowed(&self) -> BoxFuture<'_, bool>;
}

pub(super) struct SourceService {
    access: SnapshotAccess,
    authority: Option<Arc<dyn SourceAuthority>>,
    #[cfg(test)]
    pub after_access: Option<Arc<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>>,
}

impl SourceService {
    pub(super) fn new(snapshot: &SnapshotHandle) -> Self {
        Self {
            access: snapshot.access(AccessLimits::default()),
            authority: None,
            #[cfg(test)]
            after_access: None,
        }
    }

    pub(super) fn with_authority(mut self, authority: Arc<dyn SourceAuthority>) -> Self {
        self.authority = Some(authority);
        self
    }

    pub(super) async fn execute(
        &self,
        name: &str,
        arguments: &str,
    ) -> Result<Value, FunctionCallError> {
        self.check().await?;
        let result = self.execute_unchecked(name, arguments).await;
        #[cfg(test)]
        if let Some(hook) = &self.after_access {
            hook().await;
        }
        // An in-flight read never grants authority to return after retirement.
        self.check().await?;
        result
    }

    async fn check(&self) -> Result<(), FunctionCallError> {
        if let Some(authority) = &self.authority
            && !authority.allowed().await
        {
            return Err(FunctionCallError::RespondToModel(
                "reviewer attempt is stopped".to_owned(),
            ));
        }
        Ok(())
    }
}

pub(super) fn function(name: &str) -> anyhow::Result<ResponsesApiTool> {
    anyhow::ensure!(TOOL_NAMES.contains(&name), "unknown source tool");
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
    Ok(ResponsesApiTool {
        name: name.to_string(),
        description: format!(
            "{name} immutable manifest-scoped audit source. Byte offsets are zero based."
        ),
        strict: true,
        parameters,
        output_schema: None,
        defer_loading: None,
    })
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

impl SourceService {
    async fn execute_unchecked(
        &self,
        name: &str,
        arguments: &str,
    ) -> Result<Value, FunctionCallError> {
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
        match name {
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
            "search" => {
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
            _ => Err(FunctionCallError::RespondToModel(
                "unknown source tool".to_owned(),
            )),
        }
    }
}

fn parse<T: serde::de::DeserializeOwned>(arguments: &str) -> Result<T, FunctionCallError> {
    serde_json::from_str(arguments)
        .map_err(|_| FunctionCallError::RespondToModel("invalid source tool arguments".to_string()))
}

#[cfg(all(test, unix))]
mod tests;
