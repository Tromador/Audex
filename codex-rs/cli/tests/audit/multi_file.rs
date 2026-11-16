use super::*;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repeated_files_reach_one_two_reviewer_audit_with_cross_file_tools_and_citations()
-> Result<()> {
    let server = responses::start_mock_server().await;
    let generations = Arc::new(Mutex::new(HashMap::<String, usize>::new()));
    let observed = generations.clone();
    Mock::given(method("POST")).and(path("/v1/responses")).respond_with(move |request: &wiremock::Request| {
        let body: Value = request.body_json().unwrap();
        let assignment = body["input"].as_array().unwrap().iter()
            .flat_map(|item| item["content"].as_array().into_iter().flatten())
            .filter_map(|part| serde_json::from_str::<Value>(part["text"].as_str()?).ok())
            .find(|value| value.get("assignment_id").is_some()).unwrap();
        let mut counts = observed.lock().unwrap();
        let count = counts.entry(assignment["assignment_id"].as_str().unwrap().to_owned()).or_default();
        let paths = ["client/shared.rs", "server/shared.rs", "main.rs"];
        let events = match *count {
            0 => vec![responses::ev_function_call_with_namespace("list", "audex_source", "list", "{}"), responses::ev_completed("listed")],
            1..=3 => vec![responses::ev_function_call_with_namespace(&format!("read-{count}"), "audex_source", "read", &json!({"path":paths[*count-1],"offset":0,"max_bytes":100}).to_string()), responses::ev_completed("read")],
            4 => vec![responses::ev_function_call_with_namespace("search", "audex_source", "search", r#"{"query":"needle","path":null,"max_results":10}"#), responses::ev_completed("searched")],
            5 => {
                let text = body["input"].to_string();
                for marker in ["needle client", "needle server", "needle main"] { assert!(text.contains(marker)); }
                let result = json!({"schema_version":1,"snapshot_id":assignment["snapshot_id"],"assignment_id":assignment["assignment_id"],
                    "summary":"Cross-file fixture opinion", "findings":[{"severity":"high","confidence":"high","title":"Cross-file fixture defect", "mechanism":"Related files disagree", "impact":"Wrong result", "remediation":null,
                    "evidence":paths.iter().map(|path| json!({"description":"Captured source", "path":path,"start_byte":0,"end_byte":6})).collect::<Vec<_>>()}]});
                vec![responses::ev_assistant_message("answer", &result.to_string()), responses::ev_completed("final")]
            }
            _ => panic!("unexpected extra generation"),
        };
        *count += 1;
        ResponseTemplate::new(200).insert_header("content-type", "text/event-stream").insert_header("openai-model", "audit-test").set_body_string(responses::sse(events))
    }).mount(&server).await;
    let daemon = configured(&server)?;
    let selected = [
        ("client/shared.rs", "needle client\n"),
        ("server/shared.rs", "needle server\n"),
        ("main.rs", "needle main\n"),
    ];
    let mut command = audit(&daemon);
    for (name, bytes) in selected {
        let file = daemon.home.path().join(name);
        std::fs::create_dir_all(file.parent().unwrap())?;
        std::fs::write(&file, bytes)?;
        command.arg("--file").arg(file);
    }
    daemon.lifecycle("start")?;
    let output = command
        .args([
            "--reviewer",
            "audit_fixture:audit-test",
            "--reviewer",
            "audit_fixture:audit-test",
            "--request-key",
            "multi-file",
        ])
        .output()?;
    check(&output, 0)?;
    let disclosure = String::from_utf8_lossy(&output.stderr);
    let report = String::from_utf8_lossy(&output.stdout);
    for (name, _) in selected {
        assert!(disclosure.contains(daemon.home.path().join(name).to_str().unwrap()));
        assert!(disclosure.contains(&format!("snapshot name {name}")));
        assert!(report.contains(name));
    }
    assert!(disclosure.contains("1. audit-test") && disclosure.contains("2. audit-test"));
    assert!(report.contains("Cross-file fixture defect"));
    assert_eq!(
        generations
            .lock()
            .unwrap()
            .values()
            .copied()
            .collect::<Vec<_>>(),
        vec![6, 6]
    );
    let id = run_id(&output)?;
    for (name, _) in selected {
        std::fs::write(daemon.home.path().join(name), "LIVE CHANGED")?;
    }
    daemon.lifecycle("restart")?;
    let retained = audit(&daemon).args(["read", &id]).output()?;
    check(&retained, 0)?;
    assert!(String::from_utf8_lossy(&retained.stdout).contains("Cross-file fixture defect"));
    assert_eq!(server.received_requests().await.unwrap().len(), 12);
    Ok(())
}
