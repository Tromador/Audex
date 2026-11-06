//! An injected oversized list at the transport boundary. V1 source capture only
//! admits one entry; this fixture does not change that manifest contract.
use super::*;
use crate::audex::capture::CaptureHandle;
use codex_extension_api::{
    ExtensionData, ExtensionRegistryBuilder, ToolCall, ToolContributor, ToolExecutor,
    ToolExecutorFuture, ToolName, ToolSpec,
};
use codex_protocol::protocol::Op;
use codex_tools::{JsonToolOutput, ToolExposure, ToolOutput};

#[derive(Clone)]
struct InjectedList {
    value: Value,
    capture: CaptureHandle,
    full: bool,
}
impl ToolContributor for InjectedList {
    fn tools(
        &self,
        _: &ExtensionData,
        _: &ExtensionData,
    ) -> Vec<Arc<dyn for<'call> ToolExecutor<ToolCall<'call>>>> {
        vec![Arc::new(self.clone())]
    }
}
impl<'call> ToolExecutor<ToolCall<'call>> for InjectedList {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("audex_source__list")
    }
    fn spec(&self) -> ToolSpec {
        let mut spec = crate::audex::source::function("list").unwrap();
        spec.name = "audex_source__list".into();
        ToolSpec::Function(spec)
    }
    fn exposure(&self) -> ToolExposure {
        ToolExposure::DirectModelOnly
    }
    fn handle<'a>(&'a self, call: ToolCall<'call>) -> ToolExecutorFuture<'a>
    where
        'call: 'a,
    {
        Box::pin(async move {
            assert!(self.capture.allowed().await);
            self.capture
                .tool(
                    &call.turn_id,
                    &call.call_id,
                    "tool_start",
                    json!({"tool":"list","arguments":{}}).to_string(),
                )
                .await
                .unwrap();
            self.capture
                .tool(
                    &call.turn_id,
                    &call.call_id,
                    "tool_result",
                    self.value.to_string(),
                )
                .await
                .unwrap();
            let output = JsonToolOutput::new(self.value.clone()).with_external_context();
            let output = if self.full {
                output.with_full_output()
            } else {
                output
            };
            Ok(Box::new(output) as Box<dyn ToolOutput>)
        })
    }
}

#[test]
fn oversized_list_transport_matches_the_retained_payload_with_full_output() -> anyhow::Result<()> {
    run_test_with_large_stack("synthetic-list-delivery", || async {
        for full in [false, true] {
            let temp = tempfile::tempdir()?;
            let (controller, fake, _) = fixture(&temp).await;
            let id = start(&controller, "synthetic-list").await;
            controller.tick().await?;
            let store = controller.store().await?;
            let run = store.load_run(&id).await?;
            let capture = super::super::super::attempts::capture(&controller, &fake, &run).await?;
            let snapshot = controller
                .snapshots()
                .open(&SnapshotId::parse(run.snapshot_id.clone())?)
                .await?;
            let source = SourceService::new(&snapshot).execute("list", "{}").await?;
            let entries = (0..1200)
                .map(|i| {
                    let mut entry = source["entries"][0].clone();
                    entry["path"] = json!(format!("synthetic-{i:04}.rs"));
                    entry
                })
                .collect::<Vec<_>>();
            let value = json!({"entries":entries});
            assert!(value.to_string().len() > 48000);
            assert!(value.to_string().len() < crate::audex::source::MAX_RESULT_BYTES);
            let server = responses::start_mock_server().await;
            let mock = responses::mount_sse_sequence(
                &server,
                vec![
                    sse_for_model(
                        "gpt-6.1-sol",
                        vec![
                            ev_response_created("list-generation"),
                            responses::ev_function_call("large-list", "audex_source__list", "{}"),
                            ev_completed("list-generation"),
                        ],
                    ),
                    sse_for_model(
                        "gpt-6.1-sol",
                        vec![
                            ev_assistant_message("final", "DONE"),
                            ev_completed("final-generation"),
                        ],
                    ),
                ],
            )
            .await;
            let mut registry = ExtensionRegistryBuilder::<Config>::new();
            registry.tool_contributor(Arc::new(InjectedList {
                value: value.clone(),
                capture: capture.clone(),
                full,
            }));
            let mut builder = core_test_support::test_codex::test_codex()
                .with_model("gpt-6.1-sol")
                .with_extensions(Arc::new(registry.build()));
            let test = builder.build(&server).await?;
            test.submit_turn("Exercise the injected list result.")
                .await?;
            capture.flush().await?;
            let requests = mock.requests();
            assert_eq!(requests.len(), 2);
            let request = requests[1].body_json();
            let received = request["input"]
                .as_array()
                .unwrap()
                .iter()
                .find(|v| v["type"] == "function_call_output" && v["call_id"] == "large-list")
                .unwrap()["output"]
                .as_str()
                .unwrap();
            let activity = store.activity(&run.assignment_id, 0, 100).await?;
            let retained = &activity
                .iter()
                .find(|e| e.kind == "tool_result")
                .unwrap()
                .payload;
            assert_eq!(serde_json::from_str::<Value>(retained)?, value);
            assert_eq!(received == value.to_string(), full);
            if full {
                assert_eq!(
                    serde_json::from_str::<Value>(received)?["entries"]
                        .as_array()
                        .unwrap()
                        .len(),
                    1200
                );
            }
            let evidence = json!({"synthetic_list":true,"full_output":full,"entries":1200,"produced_bytes":value.to_string().len(),"model_bytes":received.len(),"retained_bytes":retained.len(),"exact_delivery":received == retained});
            println!("{evidence}");
            if let Ok(root) = std::env::var("AUDEX_FIDELITY_EVIDENCE") {
                let root =
                    PathBuf::from(root).join(if full { "list-after" } else { "list-before" });
                std::fs::create_dir_all(&root)?;
                std::fs::write(root.join("source-single-entry.json"), source.to_string())?;
                std::fs::write(root.join("injected-list.json"), value.to_string())?;
                std::fs::write(root.join("request.json"), request.to_string())?;
                std::fs::write(root.join("retained.json"), retained)?;
                std::fs::write(root.join("comparison.json"), evidence.to_string())?;
            }
            test.codex.submit(Op::Shutdown).await?;
        }
        Ok(())
    })
}
