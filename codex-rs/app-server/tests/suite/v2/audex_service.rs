//! Offline production RPC, native reviewer and official-client launcher boundaries.
use app_test_support::{MockResponsesConfig, TestAppServer};
use codex_app_server_protocol::{
    AuditListPage, AuditReadResult, AuditRunResponse, ClientRequest, ThreadClaudeParams,
    ThreadClaudeResponse, ThreadReadParams, ThreadReadResponse, ThreadStartParams,
};
use core_test_support::responses;
use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;
use wiremock::{
    Mock, ResponseTemplate,
    matchers::{method, path},
};

#[tokio::test]
async fn file_backed_two_reviewer_rpc_and_claude_audit_followup() -> anyhow::Result<()> {
    let mock = responses::start_mock_server().await;
    let home = tempfile::tempdir()?;
    let root = home.path();
    for name in ["audex", "source", "profile", "runtime", "client"] {
        std::fs::create_dir(root.join(name))?;
    }
    for name in ["profile", "runtime"] {
        std::fs::set_permissions(root.join(name), std::fs::Permissions::from_mode(0o700))?;
    }
    std::fs::write(root.join("source/example.rs"), "fn example() {}\n")?;
    std::fs::write(root.join("AUDEX.md"), "GUIDANCE_ONCE_SENTINEL")?;
    std::fs::write(
        root.join("source/AGENTS.md"),
        "HOSTILE_PROJECT_INSTRUCTIONS",
    )?;
    std::fs::write(root.join("profile/.credentials.json"), "synthetic OAuth")?;
    let executable = root.join("client/claude");
    std::fs::write(
        &executable,
        include_str!("../../../src/audex/service/claude/fake_claude.py"),
    )?;
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700))?;
    std::fs::write(root.join("client/fixture.json"), r#"{"mode":"finish"}"#)?;
    std::fs::write(root.join("client/stderr.bin"), "fake diagnostic\n")?;
    std::fs::write(
        root.join("client/stdout.bin"),
        format!(
            "{}\n",
            json!({"type":"result","subtype":"success","is_error":false,"result":"literal Claude audit","structured_output":{"summary":"Claude findings summary","findings":[]}})
        ),
    )?;
    std::fs::write(root.join("audex/reviewer-backends.json"),json!({"version":1,"presets":{"default":["native:mock-model","claude_subscription:claude-fixture"]},"claudeSubscription":{"executable":executable,"profile":root.join("profile"),"runtimeRoot":root.join("runtime")}}).to_string())?;
    MockResponsesConfig::new(&mock.uri())
        .with_root_config("developer_instructions = \"NORMAL_CODEX_INSTRUCTION\"")
        .write(root)?;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(|request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            assert!(!body.to_string().contains("NORMAL_CODEX_INSTRUCTION"));
            assert!(!body.to_string().contains("HOSTILE_PROJECT_INSTRUCTIONS"));
            assert_eq!(
                body.to_string().matches("GUIDANCE_ONCE_SENTINEL").count(),
                1
            );
            assert!(body.to_string().contains("exec_command"));
            assert!(!body.to_string().contains("audex_source"));
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(responses::sse(vec![
                    responses::ev_assistant_message(
                        "native",
                        r#"{"summary":"Native findings summary","findings":[]}"#,
                    ),
                    responses::ev_completed("native-done"),
                ]))
        })
        .expect(1)
        .mount(&mock)
        .await;
    let mut client = TestAppServer::builder()
        .with_codex_home(root)
        .build_initialized()
        .await?;
    let page: AuditListPage = client
        .request(|id| ClientRequest::AuditList {
            request_id: id,
            params: serde_json::from_value(json!({"limit":1})).unwrap(),
        })
        .await?;
    assert_eq!(page.presets["default"].len(), 2);
    let started:AuditRunResponse=client.request(|id|ClientRequest::AuditStart{request_id:id,params:serde_json::from_value(json!({"input":{"type":"paths","paths":[root.join("source")]},"cwd":root,"brief":"Offline fixture","guidance":"AUDEX.md","reviewers":[{"route":"native","model":"mock-model"},{"route":"claude_subscription","model":"claude-fixture"}]})).unwrap()}).await?;
    let run_id = started.run.run_id;
    let notification = tokio::time::timeout(
        Duration::from_secs(60),
        client.read_stream_until_matching_notification("audit completion", |n| {
            n.method == "audit/progress"
                && n.params
                    .as_ref()
                    .is_some_and(|p| p["runId"] == run_id && p["complete"] == true)
        }),
    )
    .await??;
    assert_eq!(notification.params.unwrap()["complete"], true);
    let read: AuditReadResult = client
        .request(|id| ClientRequest::AuditRead {
            request_id: id,
            params: serde_json::from_value(json!({"runId":run_id})).unwrap(),
        })
        .await?;
    assert!(read.run.complete);
    assert_eq!(read.run.reviewers.len(), 2);
    assert!(
        read.run.reviewers.iter().all(|s| s.state == "succeeded"),
        "{:?}",
        read.run.reviewers
    );
    assert!(
        read.run
            .summary
            .unwrap()
            .contains("Native findings summary")
    );
    assert!(
        root.join("audex/runs")
            .join(&run_id)
            .join("run.json")
            .exists()
    );
    assert!(!root.join("audex/snapshots").exists());
    let ordinary = client.start_thread(ThreadStartParams::default()).await?;
    assert_ne!(
        ordinary.thread.thread_source,
        Some(codex_app_server_protocol::ThreadSource::Feature(
            "audex-reviewer".into()
        ))
    );
    // The follow-up uses the same S3 fake, with ordinary conversational text.
    std::fs::write(
        root.join("client/stdout.bin"),
        format!(
            "{}\n",
            json!({"type":"result","subtype":"success","is_error":false,"result":"Claude follow-up: retained evidence discussed"})
        ),
    )?;
    let turn: ThreadClaudeResponse = client
        .request(|id| ClientRequest::ThreadClaude {
            request_id: id,
            params: ThreadClaudeParams {
                thread_id: ordinary.thread.id.clone(),
                model: Some("claude-followup".into()),
                message: "Discuss the native review".into(),
                audit: Some(run_id.clone()),
            },
        })
        .await?;
    tokio::time::timeout(
        Duration::from_secs(30),
        client.read_stream_until_matching_notification("Claude completion", |n| {
            n.method == "turn/completed"
                && n.params
                    .as_ref()
                    .is_some_and(|p| p["turn"]["id"] == turn.turn_id)
        }),
    )
    .await??;
    let conversation: ThreadReadResponse = client
        .request(|id| ClientRequest::ThreadRead {
            request_id: id,
            params: ThreadReadParams {
                thread_id: ordinary.thread.id,
                include_turns: true,
            },
        })
        .await?;
    assert!(serde_json::to_string(&conversation)?.contains("Claude follow-up"));
    let calls: Vec<Value> = std::fs::read_to_string(root.join("client/calls.jsonl"))?
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()?;
    assert_eq!(calls.len(), 2);
    let prompt = calls[0]["prompt"].as_str().unwrap();
    assert_eq!(prompt.matches("GUIDANCE_ONCE_SENTINEL").count(), 1);
    let envelope: Value = serde_json::from_str(prompt)?;
    let assignment: Value = serde_json::from_str(envelope["message"]["content"].as_str().unwrap())?;
    assert_eq!(assignment["brief"], "Offline fixture");
    assert_eq!(assignment["guidance"], "GUIDANCE_ONCE_SENTINEL");
    let args = calls[1]["args"].as_array().unwrap();
    // An audit follow-up reads retained results and source with reference tools;
    // only a plain /claude conversation is restricted.
    assert!(!args.contains(&json!("--restricted")));
    assert!(args.contains(&json!(root.join("audex/runs").join(&run_id))));
    assert!(args.contains(&json!("Read,Grep,Glob,WebSearch")));
    assert!(!args.contains(&json!("--mcp-config")));
    assert!(
        !calls[1]["prompt"]
            .as_str()
            .unwrap()
            .contains("may have changed")
    );
    assert!(calls[1]["prompt"].as_str().unwrap().contains("result.json"));
    assert!(!serde_json::to_string(&conversation)?.contains("may have changed"));
    for reviewer in ["1", "2"] {
        let report: codex_app_server_protocol::AuditReadResult = client
            .request(|id| ClientRequest::AuditRead {
                request_id: id,
                params: serde_json::from_value(
                    json!({"runId":run_id,"view":"report","reviewer":reviewer}),
                )
                .unwrap(),
            })
            .await?;
        let markdown = report.data.unwrap()["markdown"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(markdown.starts_with("# Review — reviewer-"));
        assert!(markdown.contains("## Summary"));
        assert!(!markdown.contains("Source is live"));
    }
    let deleted: codex_app_server_protocol::AuditDeleteResponse = client
        .request(|id| ClientRequest::AuditDelete {
            request_id: id,
            params: codex_app_server_protocol::AuditDeleteRequest {
                run_id: run_id.clone(),
            },
        })
        .await?;
    assert_eq!(deleted.run_id, run_id);
    assert!(!root.join("audex/runs").join(&run_id).exists());
    assert_eq!(
        std::fs::read_to_string(root.join("source/example.rs"))?,
        "fn example() {}\n"
    );
    let page: AuditListPage = client
        .request(|id| ClientRequest::AuditList {
            request_id: id,
            params: serde_json::from_value(json!({})).unwrap(),
        })
        .await?;
    assert!(page.runs.is_empty());
    Ok(())
}
