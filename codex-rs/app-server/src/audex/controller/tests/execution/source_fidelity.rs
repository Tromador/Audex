//! Source executor -> native provider request -> retained source evidence.
use super::*;

mod synthetic_list;
use crate::audex::source::SourceService;
use std::path::PathBuf;

#[test]
fn native_source_outputs_match_the_provider_request() -> anyhow::Result<()> {
    run_test_with_large_stack("source-output-fidelity", || async {
        let mut observations = Vec::new();
        for websocket in [false, true] {
            for (case, bytes, max_bytes) in [
                ("ascii-61440", vec![b'x'; 61440], 61440),
                ("ascii-65536", vec![b'x'; 65536], 65536),
                ("utf8-65536", "😀".repeat(16384).into_bytes(), 65536),
                ("utf8-split", "😀".repeat(16384).into_bytes(), 65535),
                (
                    "binary-65536",
                    (0..65536).map(|n| (n % 256) as u8).collect(),
                    65536,
                ),
                ("json-expansion", vec![0; 65536], 65536),
                ("at-bound-minus-one", vec![b'x'; 47830], 65536),
                ("at-bound", vec![b'x'; 47831], 65536),
                ("at-bound-plus-one", vec![b'x'; 47832], 65536),
                ("near-bound-above", vec![b'x'; 48000], 65536),
            ] {
                let server = responses::start_mock_server().await;
                let temp = tempfile::tempdir()?;
                let file = temp.path().join("input");
                std::fs::write(&file, &bytes)?;
                let arguments =
                    json!({"path":"example.rs","offset":0,"max_bytes":max_bytes}).to_string();
                let mut created = ev_response_created("read-generation");
                created["response"]["model"] = json!("gpt-6.1-sol");
                let mut final_created = ev_response_created("final-generation");
                final_created["response"]["model"] = json!("gpt-6.1-sol");
                let generations = vec![
                    vec![
                        created,
                        ev_function_call_with_namespace(
                            "source-read",
                            "audex_source",
                            "read",
                            &arguments,
                        ),
                        ev_completed("read-generation"),
                    ],
                    vec![
                        final_created,
                        ev_assistant_message("opinion", "Source delivery fixture opinion."),
                        ev_completed("final-generation"),
                    ],
                ];
                let ws = if websocket {
                    Some(
                        responses::start_websocket_server_with_headers(vec![
                            responses::WebSocketConnectionConfig {
                                requests: generations.clone(),
                                response_headers: Vec::new(),
                                accept_delay: None,
                                close_after_requests: false,
                            },
                        ])
                        .await,
                    )
                } else {
                    None
                };
                let url = ws
                    .as_ref()
                    .map(|s| s.uri().to_owned())
                    .unwrap_or_else(|| server.uri());
                let (controller, threads) = fixture_with_transport(
                    &temp,
                    &url,
                    CodexAuth::from_api_key("local-dummy"),
                    false,
                    admission::native_reader(),
                    crate::audex::dispatch::DispatchTestHooks::default(),
                    websocket,
                )
                .await;
                let id = api(
                    &controller,
                    "audit/start",
                    json!({
                        "requestKey":case,"source":{"type":"file","name":"example.rs","path":file},
                        "brief":"Review the assigned source", "model":"gpt-6.1-sol",
                        "provider":controller.config.model_provider_id,
                    }),
                )
                .await
                .map_err(|e| anyhow::anyhow!("{e:?}"))?["run"]["runId"]
                    .as_str()
                    .unwrap()
                    .to_owned();
                let store = controller.store().await?;
                let run = store.load_run(&id).await?;
                let snapshot = controller
                    .snapshots()
                    .open(&SnapshotId::parse(run.snapshot_id.clone())?)
                    .await?;
                let produced = SourceService::new(&snapshot)
                    .execute("read", &arguments)
                    .await?;
                let mock = if websocket {
                    None
                } else {
                    Some(
                        responses::mount_sse_sequence(
                            &server,
                            generations
                                .into_iter()
                                .map(|events| sse_for_model("gpt-6.1-sol", events))
                                .collect(),
                        )
                        .await,
                    )
                };
                let view = finished(&controller, &id).await;
                assert_eq!(view["phase"], "completed", "{view}");
                let requests = if let Some(ws) = &ws {
                    ws.connections()
                        .iter()
                        .flatten()
                        .map(|r| r.body_json())
                        .collect::<Vec<_>>()
                } else {
                    mock.as_ref()
                        .unwrap()
                        .requests()
                        .iter()
                        .map(|r| r.body_json())
                        .collect::<Vec<_>>()
                };
                assert_eq!(requests.len(), 2);
                let request = requests[1].clone();
                let received = request["input"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|v| v["type"] == "function_call_output" && v["call_id"] == "source-read")
                    .expect("actual provider tool result")["output"]
                    .as_str()
                    .unwrap()
                    .to_owned();
                let activity = store.activity(&run.assignment_id, 0, 100).await?;
                let retained = activity
                    .iter()
                    .find(|e| e.kind == "tool_result" && e.item_id == "source-read")
                    .expect("retained source result")
                    .payload
                    .clone();
                assert_eq!(serde_json::from_str::<Value>(&retained)?, produced);
                let observation = json!({"case":case,"websocket":websocket,"source_bytes":bytes.len(),
                "produced_bytes":produced.to_string().len(),"received_bytes":received.len(),
                "retained_bytes":retained.len(),"exact_delivery":received == produced.to_string(),
                "received_json_valid":serde_json::from_str::<Value>(&received).is_ok(),
                "encoding":produced["encoding"],"end_offset":produced["end_offset"],"eof":produced["eof"]});
                println!("{observation}");
                if let Ok(root) = std::env::var("AUDEX_FIDELITY_EVIDENCE") {
                    let root = PathBuf::from(root)
                        .join(if websocket { "websocket" } else { "http" })
                        .join(case);
                    std::fs::create_dir_all(&root)?;
                    std::fs::write(root.join("source.json"), produced.to_string())?;
                    std::fs::write(root.join("model-output.txt"), received)?;
                    std::fs::write(root.join("retained.json"), retained)?;
                    std::fs::write(root.join("request.json"), request.to_string())?;
                    std::fs::write(root.join("comparison.json"), observation.to_string())?;
                }
                observations.push(observation);
                threads
                    .shutdown_all_threads_bounded(Duration::from_secs(5))
                    .await;
                if let Some(ws) = ws {
                    ws.shutdown().await;
                }
            }
        }
        assert!(
            observations.iter().all(|v| v["exact_delivery"] == true),
            "{observations:?}"
        );
        Ok(())
    })
}
