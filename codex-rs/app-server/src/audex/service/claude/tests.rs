use super::*;
use crate::audex::backends::{ClaudeApi, ClaudeSubscription, CredentialReference};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::time::Duration;

struct Fixture {
    temp: tempfile::TempDir,
    settings: BackendSettings,
    sources: Vec<PathBuf>,
    events: Arc<Mutex<Vec<Event>>>,
}
impl Fixture {
    fn new() -> Result<Self> {
        let temp = tempfile::tempdir()?;
        let root = temp.path();
        for name in ["source", "context", "runtime", "profile"] {
            fs::DirBuilder::new().mode(0o700).create(root.join(name))?;
        }
        for name in ["one.rs", "two.rs"] {
            fs::write(root.join("source").join(name), "one\ntwo\n")?;
        }
        fs::write(root.join("context/caller.rs"), "caller\n")?;
        let executable = root.join("claude");
        fs::write(&executable, include_str!("fake_claude.py"))?;
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700))?;
        // These files must be neither inspected nor rewritten by the launcher.
        fs::write(
            root.join("profile/settings.json"),
            "hostile settings; deliberately not JSON",
        )?;
        fs::write(root.join("profile/.credentials.json"), "fake OAuth marker")?;
        fs::write(root.join("api-key"), "fake-key\n")?;
        let executable = executable.display().to_string();
        let runtime_root = root.join("runtime").display().to_string();
        let settings = BackendSettings {
            claude_subscription: Some(ClaudeSubscription {
                executable: executable.clone(),
                client_version: "must-not-probe".into(),
                profile: root.join("profile").display().to_string(),
                runtime_root: runtime_root.clone(),
            }),
            claude_api: Some(ClaudeApi {
                executable,
                client_version: "must-not-probe".into(),
                runtime_root,
                credential: CredentialReference::File(root.join("api-key").display().to_string()),
                upstream: "https://api.anthropic.com".into(),
            }),
            ..Default::default()
        };
        let sources = vec![root.join("source"), root.join("context")];
        let fixture = Self {
            temp,
            settings,
            sources,
            events: Arc::new(Mutex::new(Vec::new())),
        };
        fixture.script(
            &result(
                Some("literal final\n"),
                Some(json!({"summary":"structured summary","findings":[]})),
            ),
            b"diagnostic\xff\r\n",
            "finish",
            0,
        )?;
        Ok(fixture)
    }
    fn root(&self) -> &Path {
        self.temp.path()
    }
    fn script(&self, stdout: &[u8], stderr: &[u8], mode: &str, exit: i32) -> Result<()> {
        fs::write(self.root().join("stdout.bin"), stdout)?;
        fs::write(self.root().join("stderr.bin"), stderr)?;
        record::write_json(
            &self.root().join("fixture.json"),
            &json!({"mode":mode,"exit":exit}),
        )
    }
    fn service(&self) -> Result<AuditService> {
        let events = self.events.clone();
        AuditService::open(
            &self.root().join("codex-home"),
            Arc::new(ClaudeReviewer::new(self.settings.clone())),
            Arc::new(move |event| events.lock().unwrap().push(event)),
        )
    }
    fn request(&self, route: &str, input: Input) -> StartRequest {
        StartRequest {
            input,
            brief: "Inspect the named targets\nKeep this exact brief".into(),
            guidance: Some("Operator guidance".into()),
            reviewers: vec![selection(route)],
        }
    }
    fn calls(&self) -> Result<Vec<Value>> {
        Ok(fs::read_to_string(self.root().join("calls.jsonl"))?
            .lines()
            .map(serde_json::from_str)
            .collect::<std::result::Result<_, _>>()?)
    }
    fn attempts(&self, service: &AuditService, id: RunId) -> Result<Vec<(PathBuf, AttemptStatus)>> {
        record::attempts(&record::reviewer_dir(
            &service.run_directory(id),
            0,
            &selection("claude_subscription"),
        ))
    }
    fn cleaned(&self) -> Result<()> {
        assert!(fs::read_dir(self.root().join("runtime"))?.next().is_none());
        Ok(())
    }
}
fn selection(route: &str) -> ReviewerSelection {
    ReviewerSelection {
        label: "claude".into(),
        route: route.into(),
        model: "requested-model".into(),
        effort: None,
        permission_summary: "unset".into(),
    }
}
fn event(value: Value) -> Vec<u8> {
    format!("{}\n", value).into_bytes()
}
fn result(text: Option<&str>, structured: Option<Value>) -> Vec<u8> {
    let mut value = json!({"type":"result", "subtype":"success", "is_error":false,
        "model":"reported-model", "usage":{"input_tokens":20,"output_tokens":7,"cache_read_input_tokens":9,"cache_creation_input_tokens":3},
        "modelUsage":{"reported-model":{"inputTokens":20,"outputTokens":7,"costUSD":0.123}}, "total_cost_usd":0.123});
    if let Some(text) = text {
        value["result"] = json!(text);
    }
    if let Some(structured) = structured {
        value["structured_output"] = structured;
    }
    event(value)
}
async fn finished(service: &mut AuditService, id: RunId) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(20), service.wait(id)).await??;
    Ok(())
}
async fn wait_file(path: &Path) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    Ok(())
}

fn expected_args(
    model: &str,
    rules: &str,
    directories: &[PathBuf],
    run: Option<&Path>,
    api: bool,
) -> Vec<String> {
    let mut args: Vec<String> = [
        "-p",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--model",
        model,
        "--tools",
        "Read,Grep,Glob,WebSearch",
        "--setting-sources",
        "user",
        "--allowedTools",
        "Read,Grep,Glob,WebSearch",
        "--permission-mode",
        "dontAsk",
        "--disable-slash-commands",
        "--no-session-persistence",
        "--settings",
        r#"{"disableAllHooks":true,"permissions":{"blockReadsOutsideWorkingDirectories":true}}"#,
        "--system-prompt",
        rules,
        "--strict-mcp-config",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    if let Some(run) = run {
        args.extend(["--add-dir".into(), run.display().to_string()]);
        for path in directories {
            args.extend(["--add-dir".into(), path.display().to_string()]);
        }
    } else {
        args.extend([
            "--json-schema".into(),
            super::super::output_schema().to_string(),
        ]);
        for path in &directories[1..] {
            args.extend(["--add-dir".into(), path.display().to_string()]);
        }
    }
    if api {
        args.push("--bare".into());
    }
    args
}
fn args_without_mcp(args: &Value) -> Result<Value> {
    let mut args = args.as_array().context("Claude arguments are not an array")?.clone();
    if let Some(index) = args.iter().position(|arg| arg == "--mcp-config") {
        let path = args.get(index + 1).and_then(Value::as_str).context("MCP config path missing")?;
        ensure!(Path::new(path).file_name().is_some_and(|name| name == "mcp.json"), "MCP configuration must be passed as a temporary file");
        args.drain(index..index + 2);
        args.retain(|arg| !arg.as_str().is_some_and(|value| value.starts_with("mcp__")));
    }
    Ok(Value::Array(args))
}

fn assert_environment(call: &Value, fixture: &Fixture, api_key: Option<&str>) {
    let mut expected = json!({"PATH":"/usr/bin:/bin", "LANG":"C.UTF-8", "TERM":"dumb",
        "CLAUDE_CODE_DISABLE_CLAUDE_MDS":"1", "CLAUDE_CODE_DISABLE_AUTO_MEMORY":"1",
        "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC":"1", "CLAUDE_CODE_DISABLE_TERMINAL_TITLE":"1"});
    for name in ["HOME", "TMPDIR"] {
        expected[name] = call["env"][name].clone();
        let path = Path::new(call["env"][name].as_str().unwrap());
        assert!(path.starts_with(fixture.root().join("runtime")));
        assert_eq!(call["modes"][name], 0o700);
    }
    if let Some(key) = api_key {
        expected["ANTHROPIC_API_KEY"] = json!(key);
        expected["CLAUDE_CONFIG_DIR"] = call["env"]["CLAUDE_CONFIG_DIR"].clone();
        assert!(
            Path::new(expected["CLAUDE_CONFIG_DIR"].as_str().unwrap())
                .starts_with(fixture.root().join("runtime"))
        );
        assert_eq!(call["profile_entries"], json!([]));
    } else {
        expected["CLAUDE_CONFIG_DIR"] = json!(fixture.root().join("profile"));
        assert_eq!(
            call["profile_entries"],
            json!([".credentials.json", "settings.json"])
        );
    }
    assert_eq!(call["modes"]["CLAUDE_CONFIG_DIR"], 0o700);
    assert_eq!(call["env"], expected);
    assert_eq!(call["pid"], call["pgid"]);
    assert_eq!(call["pdeathsig"], libc::SIGKILL);
}

#[test]
fn reference_mcp_selection_allows_only_named_read_tools() -> Result<()> {
    let input = json!({
        "context7": {"command": "c7"},
        "github": {"command": "gh"},
        "zen-storage": {"command": "zen"},
        "unrelated": {"command": "other"}
    });
    let servers = input.as_object().unwrap();
    let (config, allowed) = launch::select_mcp_servers(servers, false)?.unwrap();
    let parsed: Value = serde_json::from_str(&config)?;
    assert_eq!(
        parsed["mcpServers"].as_object().unwrap().keys().cloned().collect::<Vec<_>>(),
        vec!["context7", "github"]
    );
    for name in [
        "mcp__context7__resolve-library-id",
        "mcp__context7__query-docs",
        "mcp__github__fetch_file",
        "mcp__github__compare_commits",
    ] {
        assert!(allowed.contains(&name.to_owned()), "missing {name}");
    }
    for name in [
        "mcp__github__update_file",
        "mcp__github__delete_file",
        "mcp__github__merge_pull_request",
        "mcp__unrelated__search",
        "mcp__zen-storage__write_file",
    ] {
        assert!(!allowed.contains(&name.to_owned()), "unexpected {name}");
    }

    let (remote_config, remote_allowed) =
        launch::select_mcp_servers(servers, true)?.unwrap();
    let remote: Value = serde_json::from_str(&remote_config)?;
    assert!(remote["mcpServers"].get("zen-storage").is_some());
    assert!(remote_allowed.contains(&"mcp__zen-storage__read_text_file".to_owned()));
    assert!(!remote_allowed.contains(&"mcp__zen-storage__write_file".to_owned()));
    assert!(launch::select_mcp_servers(
        json!({"context7":{"command":"c7"}}).as_object().unwrap(),
        true
    ).is_err());
    Ok(())
}

#[test]
fn claude_effort_validation_accepts_supported_levels() -> Result<()> {
    let fixture = Fixture::new()?;
    for route in ["claude_subscription", "claude_api"] {
        let mut chosen = selection(route);
        for effort in ["low", "medium", "high", "xhigh", "max"] {
            chosen.effort = Some(effort.into());
            launch::validate(&fixture.settings, &chosen)?;
        }
        for effort in ["", "MAX", "ultracode", "--permission-mode"] {
            chosen.effort = Some(effort.into());
            assert!(launch::validate(&fixture.settings, &chosen).is_err());
        }
    }
    Ok(())
}

#[tokio::test]
async fn explicit_effort_is_forwarded_for_both_claude_routes() -> Result<()> {
    for route in ["claude_subscription", "claude_api"] {
        let fixture = Fixture::new()?;
        let mut request = fixture.request(route, Input::Paths(fixture.sources.clone()));
        request.reviewers[0].effort = Some("max".into());
        let mut service = fixture.service()?;
        let id = service.start(request)?;
        finished(&mut service, id).await?;
        let calls = fixture.calls()?;
        assert_eq!(calls.len(), 1);
        let args: Vec<String> = serde_json::from_value(calls[0]["args"].clone())?;
        assert!(
            args.windows(2)
                .any(|pair| pair[0] == "--effort" && pair[1] == "max")
        );
        fixture.cleaned()?;
    }
    Ok(())
}

#[tokio::test]
async fn exact_launch_for_both_routes_and_all_target_shapes() -> Result<()> {
    for route in ["claude_subscription", "claude_api"] {
        for shape in ["directory", "directories", "files", "snippet"] {
            let fixture = Fixture::new()?;
            let input = match shape {
                "directory" => Input::Paths(vec![fixture.sources[0].clone()]),
                "directories" => Input::Paths(fixture.sources.clone()),
                "files" => Input::Paths(vec![
                    fixture.sources[0].join("one.rs"),
                    fixture.sources[0].join("two.rs"),
                    fixture.sources[1].join("caller.rs"),
                ]),
                _ => Input::Snippet {
                    name: "pasted.rs".into(),
                    text: "operator paste\n".into(),
                },
            };
            let mut service = fixture.service()?;
            let id = service.start(fixture.request(route, input))?;
            finished(&mut service, id).await?;
            let run: RunRecord = record::read_json(&service.run_directory(id).join("run.json"))?;
            let calls = fixture.calls()?;
            assert_eq!(calls.len(), 1);
            let call = &calls[0];
            assert_eq!(
                args_without_mcp(&call["args"])?,
                json!(expected_args(
                    "requested-model",
                    crate::audex::reviewer::native::RULES,
                    &run.targets.working_directories,
                    None,
                    route == "claude_api"
                ))
            );
            assert_eq!(call["cwd"], json!(run.targets.working_directories[0]));
            assert_environment(
                call,
                &fixture,
                (route == "claude_api").then_some("fake-key"),
            );
            let lines: Vec<_> = call["prompt"].as_str().unwrap().lines().collect();
            assert_eq!(lines.len(), 1);
            let user: Value = serde_json::from_str(lines[0])?;
            assert_eq!(user["type"], "user");
            assert_eq!(user["message"]["role"], "user");
            let content: Value =
                serde_json::from_str(user["message"]["content"].as_str().unwrap())?;
            assert_eq!(content["targets"], json!(run.targets.paths));
            assert_eq!(
                content["working_directories"],
                json!(run.targets.working_directories)
            );
            assert_eq!(content["brief"], run.brief);
            assert_eq!(content["guidance"], json!(run.guidance));
            match shape {
                "directory" => assert_eq!(run.targets.paths, vec![fixture.sources[0].clone()]),
                "directories" => assert_eq!(run.targets.paths, fixture.sources),
                "files" => {
                    assert_eq!(run.targets.paths.len(), 3);
                    assert_eq!(run.targets.working_directories, fixture.sources);
                }
                _ => {
                    assert_eq!(
                        run.targets.working_directories,
                        vec![service.run_directory(id).join("input")]
                    );
                    assert_eq!(fs::read(&run.targets.paths[0])?, b"operator paste\n");
                }
            }
            assert_eq!(fixture.attempts(&service, id)?[0].1.state, State::Succeeded);
            fixture.cleaned()?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn api_environment_credential_reference_is_resolved_before_clearing() -> Result<()> {
    let mut fixture = Fixture::new()?;
    fixture.settings.claude_api.as_mut().unwrap().credential =
        CredentialReference::Environment("AUDEX_S3_TEST_KEY".into());
    let mut service = fixture.service()?;
    let id = service.start(fixture.request("claude_api", Input::Paths(fixture.sources.clone())))?;
    finished(&mut service, id).await?;
    assert_environment(&fixture.calls()?[0], &fixture, Some("synthetic-s3-key"));
    Ok(())
}

#[test]
fn profile_checks_are_limited_to_private_dedicated_directory() -> Result<()> {
    let mut fixture = Fixture::new()?;
    let config = fixture.settings.claude_subscription.as_mut().unwrap();
    let original = config.profile.clone();
    config.profile = fixture.temp.path().join("missing").display().to_string();
    assert!(launch::validate(&fixture.settings, &selection("claude_subscription")).is_err());
    fixture
        .settings
        .claude_subscription
        .as_mut()
        .unwrap()
        .profile = original.clone();
    fs::set_permissions(&original, fs::Permissions::from_mode(0o755))?;
    assert!(launch::validate(&fixture.settings, &selection("claude_subscription")).is_err());
    fs::set_permissions(&original, fs::Permissions::from_mode(0o700))?;
    assert!(launch::validate(&fixture.settings, &selection("claude_subscription")).is_ok());
    // The test runner supplies a task-owned HOME. Do not touch a real profile.
    let personal = PathBuf::from(std::env::var("HOME")?).join(".claude");
    ensure!(!personal.exists(), "fixture requires an empty private HOME");
    fs::DirBuilder::new().mode(0o700).create(&personal)?;
    fixture
        .settings
        .claude_subscription
        .as_mut()
        .unwrap()
        .profile = personal.display().to_string();
    let rejected = launch::validate(&fixture.settings, &selection("claude_subscription")).is_err();
    fs::remove_dir(personal)?;
    assert!(rejected);
    Ok(())
}

#[tokio::test]
async fn verbatim_capture_structured_findings_citations_progress_model_and_accounting() -> Result<()>
{
    let fixture = Fixture::new()?;
    let finding = json!({"path":"one.rs","start_line":1,"end_line":2,"title":"Claim","severity":"high","confidence":"medium",
        "trigger":"trigger","mechanism":"mechanism","impact":"impact","evidence":"one","remediation":"fix"});
    let structured = json!({"summary":"client structured findings","findings":[finding]});
    let mut stdout =
        b"{ \"type\": \"system\", \"subtype\": \"init\", \"model\": \"init-model\" }\r\n".to_vec();
    stdout.extend(event(json!({"type":"assistant","message":{"model":"assistant-model","content":[
        {"type":"text","text":"investigating"},{"type":"tool_use","id":"a","name":"Read","input":{}},
        {"type":"tool_use","id":"b","name":"Grep","input":{}}]}})));
    let raw = "  Literal reviewer text 🦀\r\n```not-json\nkeep me\n```\n";
    stdout.extend(result(Some(raw), Some(structured)));
    let stderr = b"diagnostic\0\xff\r\n";
    fixture.script(&stdout, stderr, "finish", 0)?;
    let mut service = fixture.service()?;
    let id = service
        .start(fixture.request("claude_subscription", Input::Paths(fixture.sources.clone())))?;
    finished(&mut service, id).await?;
    let attempts = fixture.attempts(&service, id)?;
    let (directory, status) = &attempts[0];
    assert_eq!(fs::read(directory.join("transcript.jsonl"))?, stdout);
    assert_eq!(fs::read(directory.join("stderr.log"))?, stderr);
    assert_eq!(fs::read(directory.join("output.txt"))?, raw.as_bytes());
    assert_eq!(status.tool_calls, 2);
    assert_eq!(status.reviewer.model, "requested-model");
    assert_eq!(status.reported_model.as_deref(), Some("reported-model"));
    let collected: Value = record::read_json(&directory.join("result.json"))?;
    assert_eq!(collected["summary"], "client structured findings");
    assert_eq!(collected["findings"][0]["citation"], "ok");
    let usage: Value = record::read_json(&directory.join("usage.json"))?;
    assert_eq!(usage["basis"], "measured");
    assert_eq!(usage["cost_basis"], "estimated");
    assert_eq!(usage["input_tokens"], 20);
    assert_eq!(usage["output_tokens"], 7);
    assert_eq!(usage["cache_read_input_tokens"], 9);
    assert_eq!(usage["cache_creation_input_tokens"], 3);
    assert_eq!(usage["cost_usd"], 0.123);
    assert_eq!(usage["model_usage"]["reported-model"]["inputTokens"], 20);
    assert!(
        fixture
            .events
            .lock()
            .unwrap()
            .iter()
            .any(|event| matches!(event, Event::Progress { status, .. } if status.tool_calls == 2))
    );
    fixture.cleaned()?;
    Ok(())
}

#[tokio::test]
async fn fallback_preserves_fenced_text_and_structured_only_success_does_not_invent_output()
-> Result<()> {
    for structured_only in [false, true] {
        let fixture = Fixture::new()?;
        let fenced =
            "\u{feff} \r\n```json\r\n{\"summary\":\"fallback\",\"findings\":[]}\r\n```\r\n";
        let stdout = if structured_only {
            result(
                None,
                Some(json!({"summary":"structured only","findings":[]})),
            )
        } else {
            result(Some(fenced), None)
        };
        fixture.script(&stdout, b"", "finish", 0)?;
        let mut service = fixture.service()?;
        let id = service
            .start(fixture.request("claude_subscription", Input::Paths(fixture.sources.clone())))?;
        finished(&mut service, id).await?;
        let attempts = fixture.attempts(&service, id)?;
        assert_eq!(attempts.len(), 1);
        let (directory, status) = &attempts[0];
        assert_eq!(status.state, State::Succeeded);
        assert!(directory.join("result.json").exists());
        if structured_only {
            assert!(!directory.join("output.txt").exists());
        } else {
            assert_eq!(fs::read_to_string(directory.join("output.txt"))?, fenced);
        }
    }
    Ok(())
}

#[tokio::test]
async fn failures_keep_evidence_and_use_only_s1_zero_output_replacement() -> Result<()> {
    for case in [
        "malformed",
        "eof",
        "error-result",
        "unparseable",
        "bad-structured",
        "trailing-malformed",
        "wrong-output-type",
        "exit",
    ] {
        let fixture = Fixture::new()?;
        let mut stdout = match case {
            "malformed" => b"{broken\n".to_vec(),
            "eof" => event(
                json!({"type":"assistant","message":{"content":[{"type":"text","text":"partial investigation"}]}}),
            ),
            "error-result" => {
                event(json!({"type":"result","subtype":"error_during_execution","is_error":true}))
            }
            "wrong-output-type" => event(
                json!({"type":"result","subtype":"success","is_error":false,"result":{"not":"text"}}),
            ),
            "bad-structured" => result(
                Some("{\"summary\":\"must not fall back\",\"findings\":[]}"),
                Some(json!({"invalid":true})),
            ),
            _ => result(Some("unparseable final opinion"), None),
        };
        if case == "trailing-malformed" {
            stdout.extend(b"broken\n");
        }
        let has_output = matches!(
            case,
            "unparseable" | "bad-structured" | "trailing-malformed" | "exit"
        );
        fixture.script(
            &stdout,
            b"failure stderr\n",
            "finish",
            if matches!(case, "unparseable" | "exit") {
                3
            } else {
                0
            },
        )?;
        let mut service = fixture.service()?;
        let id = service
            .start(fixture.request("claude_subscription", Input::Paths(fixture.sources.clone())))?;
        finished(&mut service, id).await?;
        let attempts = fixture.attempts(&service, id)?;
        assert_eq!(attempts.len(), if has_output { 1 } else { 2 }, "{case}");
        for (index, (directory, status)) in attempts.iter().enumerate() {
            assert_eq!(
                status.state,
                if case == "bad-structured" {
                    State::Succeeded
                } else {
                    State::Failed
                },
                "{case}"
            );
            assert_eq!(
                status.trigger,
                if index == 0 {
                    Trigger::Initial
                } else {
                    Trigger::AutomaticReplacement
                }
            );
            assert_eq!(
                fs::read(directory.join("transcript.jsonl"))?,
                stdout,
                "{case}"
            );
            assert_eq!(fs::read(directory.join("stderr.log"))?, b"failure stderr\n");
            assert_eq!(directory.join("output.txt").exists(), has_output, "{case}");
            assert!(!directory.join("result.json").exists(), "{case}");
        }
        assert_eq!(fixture.calls()?.len(), attempts.len());
        fixture.cleaned()?;
    }
    Ok(())
}

#[tokio::test]
async fn record_bound_is_not_a_stream_or_record_count_bound() -> Result<()> {
    let fixture = Fixture::new()?;
    // Exceed both legacy stream/record ceilings, with small valid records.
    let record = event(json!({"type":"status","padding":"x".repeat(1024)}));
    let mut stdout = record.repeat(65_537);
    stdout.extend(result(Some("finished large stream"), None));
    fixture.script(&stdout, &vec![b'e'; 70_000], "finish", 0)?;
    let mut service = fixture.service()?;
    let id = service.start(fixture.request("claude_api", Input::Paths(fixture.sources.clone())))?;
    finished(&mut service, id).await?;
    let attempts = fixture.attempts(&service, id)?;
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].1.state, State::Succeeded);
    assert_eq!(fs::read(attempts[0].0.join("transcript.jsonl"))?, stdout);
    assert_eq!(
        fs::metadata(attempts[0].0.join("stderr.log"))?.len(),
        70_000
    );
    let oversized = vec![b'x'; stream::MAX_RECORD + 1];
    fixture.script(&oversized, b"", "finish", 0)?;
    let id = service.start(fixture.request("claude_api", Input::Paths(fixture.sources.clone())))?;
    finished(&mut service, id).await?;
    for (directory, status) in fixture.attempts(&service, id)? {
        assert_eq!(status.state, State::Failed);
        assert!(status.reason.unwrap().contains("exceeds 8 MiB"));
        assert_eq!(fs::read(directory.join("transcript.jsonl"))?, oversized);
    }
    fixture.cleaned()?;
    Ok(())
}

fn live(pid: u32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .is_some_and(|stat| {
            stat.rsplit_once(") ")
                .is_some_and(|(_, tail)| !tail.starts_with('Z'))
        })
}
#[tokio::test]
async fn cancellation_terminates_group_reaps_leader_and_preserves_partial_evidence() -> Result<()> {
    for mode in ["cooperate", "stubborn"] {
        let fixture = Fixture::new()?;
        let stdout = event(
            json!({"type":"assistant","message":{"content":[{"type":"text","text":"partial review"}]}}),
        );
        fixture.script(&stdout, b"partial diagnostics\n", mode, 0)?;
        let mut service = fixture.service()?;
        let id = service
            .start(fixture.request("claude_subscription", Input::Paths(fixture.sources.clone())))?;
        wait_file(&fixture.root().join("ready")).await?;
        service.cancel(id, None)?;
        finished(&mut service, id).await?;
        let attempts = fixture.attempts(&service, id)?;
        assert_eq!(attempts.len(), 1);
        let (directory, status) = &attempts[0];
        assert_eq!(status.state, State::Cancelled);
        assert_eq!(fs::read(directory.join("transcript.jsonl"))?, stdout);
        assert_eq!(
            fs::read(directory.join("stderr.log"))?,
            b"partial diagnostics\nSIGTERM received\n"
        );
        assert!(!directory.join("output.txt").exists());
        let pids: Vec<u32> = record::read_json(&fixture.root().join("processes.json"))?;
        assert!(
            !Path::new(&format!("/proc/{}", pids[0])).exists(),
            "leader was not reaped"
        );
        for pid in pids {
            assert!(!live(pid), "live child {pid} after {mode} cancellation");
        }
        assert_eq!(fixture.calls()?.len(), 1);
        fixture.cleaned()?;
    }
    Ok(())
}

#[tokio::test]
async fn task_abort_still_cleans_group_and_publishes_captured_bytes() -> Result<()> {
    let fixture = Fixture::new()?;
    let stdout = event(json!({"type":"status","message":"partial"}));
    fixture.script(&stdout, b"stderr", "stubborn", 0)?;
    let directory = fixture.root().join("capture");
    fs::create_dir(&directory)?;
    let launch = launch::prepare(
        &fixture.settings,
        &selection("claude_subscription"),
        &fixture.sources,
        "rules",
        json!({"brief":"test"}),
        None,
        false,
    )?;
    let capture_dir = directory.clone();
    let observed = Arc::new(tokio::sync::Notify::new());
    let observer = observed.clone();
    let task = tokio::spawn(async move {
        stream::run(launch, &capture_dir, &CancellationToken::new(), |_| {
            observer.notify_one();
            Ok(())
        })
        .await
    });
    wait_file(&fixture.root().join("ready")).await?;
    // The fixture emits before its readiness marker; wait until capture has
    // processed the record as well before aborting its future.
    observed.notified().await;
    task.abort();
    assert!(matches!(task.await, Err(error) if error.is_cancelled()));
    let pids: Vec<u32> = record::read_json(&fixture.root().join("processes.json"))?;
    tokio::time::timeout(Duration::from_secs(5), async {
        while pids.iter().any(|pid| live(*pid))
            || fs::read_dir(fixture.root().join("runtime"))
                .unwrap()
                .next()
                .is_some()
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?;
    assert_eq!(fs::read(directory.join("transcript.jsonl"))?, stdout);
    assert_eq!(fs::read(directory.join("stderr.log"))?, b"stderr");
    Ok(())
}

#[tokio::test]
async fn normal_exit_removes_remaining_group_members() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.script(&result(Some("complete"), None), b"", "leftover", 0)?;
    let mut service = fixture.service()?;
    let id = service
        .start(fixture.request("claude_subscription", Input::Paths(fixture.sources.clone())))?;
    finished(&mut service, id).await?;
    let pids: Vec<u32> = record::read_json(&fixture.root().join("processes.json"))?;
    for pid in pids {
        assert!(!live(pid));
    }
    assert_eq!(fixture.attempts(&service, id)?[0].1.state, State::Succeeded);
    fixture.cleaned()?;
    Ok(())
}

#[tokio::test]
async fn audit_conversation_launcher_exposes_results_and_live_source_without_legacy_bridge()
-> Result<()> {
    for route in ["claude_subscription", "claude_api"] {
        let fixture = Fixture::new()?;
        let mut service = fixture.service()?;
        let id = service.start(fixture.request(route, Input::Paths(fixture.sources.clone())))?;
        finished(&mut service, id).await?;
        let run_dir = service.run_directory(id);
        let legacy_summary = format!(
            "{}\nSource is live and may have changed since the review.\n",
            fs::read_to_string(run_dir.join("summary.md"))?
        );
        fs::write(run_dir.join("summary.md"), &legacy_summary)?;
        let evidence = fixture.root().join("conversation");
        fs::create_dir(&evidence)?;
        fixture.script(
            &result(Some("Ordinary follow-up prose\n"), None),
            b"",
            "finish",
            0,
        )?;
        let reviewer = ClaudeReviewer::new(fixture.settings.clone());
        let outcome = reviewer
            .converse(
                &selection(route),
                &run_dir,
                &evidence,
                json!(["prior discussion"]),
                "Check the finding against the current source",
                &CancellationToken::new(),
            )
            .await?;
        assert!(matches!(outcome.completion, Completion::Succeeded));
        assert_eq!(outcome.output.unwrap(), b"Ordinary follow-up prose\n");
        let calls = fixture.calls()?;
        assert_eq!(calls.len(), 2);
        let call = &calls[1];
        assert_eq!(
            args_without_mcp(&call["args"])?,
            json!(expected_args(
                "requested-model",
                CONVERSATION_RULES,
                &fixture.sources,
                Some(&run_dir),
                route == "claude_api"
            ))
        );
        assert_environment(
            call,
            &fixture,
            (route == "claude_api").then_some("fake-key"),
        );
        let user: Value = serde_json::from_str(call["prompt"].as_str().unwrap())?;
        let content: Value = serde_json::from_str(user["message"]["content"].as_str().unwrap())?;
        let attempt = &fixture.attempts(&service, id)?[0].0;
        assert_eq!(
            content["result_files"][0]["output"],
            json!(attempt.join("output.txt"))
        );
        assert_eq!(
            content["result_files"][0]["findings"],
            json!(attempt.join("result.json"))
        );
        assert_eq!(content["retained_run"], json!(run_dir.join("run.json")));
        assert!(content.get("source_notice").is_none());
        assert!(!content.to_string().contains("may have changed"));
        assert!(!CONVERSATION_RULES.contains("Source is live"));
        assert!(!crate::audex::reviewer::native::RULES.contains("may change"));
        assert!(CONVERSATION_RULES.contains("Agreement is not verification"));
        assert!(
            content["retained_summary"]
                .as_str()
                .unwrap()
                .contains("# Audit summary")
        );
        assert_eq!(
            fs::read_to_string(run_dir.join("summary.md"))?,
            legacy_summary
        );
        assert_eq!(content["discussion"], json!(["prior discussion"]));
        fixture.cleaned()?;
    }
    Ok(())
}

#[tokio::test]
async fn cancellation_after_final_output_keeps_it_without_replacement() -> Result<()> {
    let fixture = Fixture::new()?;
    let stdout = result(Some("literal final text before cancellation\n"), None);
    fixture.script(&stdout, b"", "stubborn", 0)?;
    let mut service = fixture.service()?;
    let id = service.start(fixture.request("claude_api", Input::Paths(fixture.sources.clone())))?;
    wait_file(&fixture.root().join("ready")).await?;
    let directory = record::reviewer_dir(&service.run_directory(id), 0, &selection("claude_api"))
        .join("attempt-1");
    wait_file(&directory.join("output.txt")).await?;
    service.cancel(id, Some(0))?;
    finished(&mut service, id).await?;
    assert_eq!(fixture.attempts(&service, id)?.len(), 1);
    assert_eq!(fixture.attempts(&service, id)?[0].1.state, State::Cancelled);
    assert_eq!(
        fs::read(directory.join("output.txt"))?,
        b"literal final text before cancellation\n"
    );
    assert_eq!(fs::read(directory.join("transcript.jsonl"))?, stdout);
    fixture.cleaned()?;
    Ok(())
}

#[tokio::test]
async fn final_record_without_newline_and_model_from_assistant_are_retained() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut stdout = event(
        json!({"type":"assistant","message":{"model":"actual-model","content":[{"type":"text","text":"answer"}]}}),
    );
    let terminal = json!({"type":"result","subtype":"success","is_error":false,"result":"answer"});
    stdout.extend(serde_json::to_vec(&terminal)?);
    fixture.script(&stdout, b"", "finish", 0)?;
    let mut service = fixture.service()?;
    let id = service
        .start(fixture.request("claude_subscription", Input::Paths(fixture.sources.clone())))?;
    finished(&mut service, id).await?;
    let attempts = fixture.attempts(&service, id)?;
    assert_eq!(
        attempts[0].1.reported_model.as_deref(),
        Some("actual-model")
    );
    assert_eq!(attempts[0].1.state, State::Succeeded);
    assert_eq!(fs::read(attempts[0].0.join("transcript.jsonl"))?, stdout);
    assert_eq!(fs::read(attempts[0].0.join("output.txt"))?, b"answer");
    Ok(())
}

#[tokio::test]
async fn api_error_results_are_diagnostics_and_s1_replaces_only_once() -> Result<()> {
    let documented = json!({"type":"result","subtype":"success","is_error":true,
        "terminal_reason":"api_error","api_error_status":429,"result":"API Error: rate limit"});
    for route in ["claude_api", "claude_subscription"] {
        for case in [
            "documented",
            "nonzero-exit",
            "structured",
            "other-subtype",
            "text-only",
            "no-description",
        ] {
            let fixture = Fixture::new()?;
            let mut terminal = documented.clone();
            if case == "structured" || case == "other-subtype" {
                // Even a schema-valid payload from an error result is diagnostic.
                terminal["structured_output"] =
                    json!({"summary":"not a contribution","findings":[]});
                terminal["model"] = json!("reported-error-model");
                terminal["usage"] = json!({"input_tokens":20,"output_tokens":7});
                terminal["total_cost_usd"] = json!(0.123);
                if case == "other-subtype" {
                    terminal["subtype"] = json!("error_during_execution");
                }
            }
            if case == "text-only" || case == "no-description" {
                let fields = terminal.as_object_mut().unwrap();
                fields.remove("terminal_reason");
                fields.remove("api_error_status");
                // Unicode verifies the short diagnostic truncates characters safely.
                terminal["result"] = json!(if case == "text-only" {
                    "é".repeat(300)
                } else {
                    String::new()
                });
            }
            let stdout = event(terminal);
            let stderr = b"API failure stderr\xff\r\n";
            fixture.script(
                &stdout,
                stderr,
                "finish",
                if case == "nonzero-exit" { 1 } else { 0 },
            )?;
            let mut service = fixture.service()?;
            let id =
                service.start(fixture.request(route, Input::Paths(fixture.sources.clone())))?;
            finished(&mut service, id).await?;
            let attempts = fixture.attempts(&service, id)?;
            assert_eq!(attempts.len(), 2, "{route} / {case}");
            assert_eq!(fixture.calls()?.len(), 2, "no third automatic attempt");
            for (index, (directory, status)) in attempts.iter().enumerate() {
                assert_eq!(status.state, State::Failed);
                assert_eq!(
                    status.trigger,
                    if index == 0 {
                        Trigger::Initial
                    } else {
                        Trigger::AutomaticReplacement
                    }
                );
                let expected = match case {
                    "text-only" => format!("Claude error: {}", "é".repeat(256)),
                    "no-description" => "Claude reported an error".into(),
                    "nonzero-exit" => "Claude error: api_error; HTTP 429; API Error: rate limit (Claude exited with exit status: 1)".into(),
                    _ => "Claude error: api_error; HTTP 429; API Error: rate limit".into(),
                };
                assert_eq!(status.reason.as_deref(), Some(expected.as_str()));
                assert!(!directory.join("output.txt").exists());
                assert!(!directory.join("result.json").exists());
                assert_eq!(fs::read(directory.join("transcript.jsonl"))?, stdout);
                assert_eq!(fs::read(directory.join("stderr.log"))?, stderr);
                if case == "structured" || case == "other-subtype" {
                    assert_eq!(
                        status.reported_model.as_deref(),
                        Some("reported-error-model")
                    );
                    let usage: Value = record::read_json(&directory.join("usage.json"))?;
                    assert_eq!(usage["input_tokens"], 20);
                    assert_eq!(usage["output_tokens"], 7);
                    assert_eq!(usage["cost_usd"], 0.123);
                    assert_eq!(usage["basis"], "measured");
                    assert_eq!(usage["cost_basis"], "estimated");
                }
            }
            let summary = fs::read_to_string(service.run_directory(id).join("summary.md"))?;
            assert!(!summary.contains("Unstructured opinion"));
            assert!(!summary.contains("Claude result: success"));
            assert!(summary.contains("claude: failed"));
            fixture.cleaned()?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn malformed_envelope_keeps_real_contribution_unless_error_is_explicit() -> Result<()> {
    for case in [
        "missing-subtype",
        "missing-error-flag",
        "invalid-error-flag",
    ] {
        let fixture = Fixture::new()?;
        let text = "A genuine reviewer contribution\n";
        let mut terminal: Value = serde_json::from_slice(&result(
            Some(text),
            Some(json!({"summary":"retained findings","findings":[]})),
        ))?;
        match case {
            "missing-subtype" => {
                terminal.as_object_mut().unwrap().remove("subtype");
            }
            "missing-error-flag" => {
                terminal.as_object_mut().unwrap().remove("is_error");
            }
            _ => {
                terminal["is_error"] = json!("invalid boolean");
            }
        }
        let stdout = event(terminal);
        fixture.script(&stdout, b"malformed envelope diagnostic", "finish", 0)?;
        let mut service = fixture.service()?;
        let id =
            service.start(fixture.request("claude_api", Input::Paths(fixture.sources.clone())))?;
        finished(&mut service, id).await?;
        let attempts = fixture.attempts(&service, id)?;
        assert_eq!(
            attempts.len(),
            1,
            "{case}: real output must prevent replacement"
        );
        assert_eq!(fixture.calls()?.len(), 1);
        let (directory, status) = &attempts[0];
        assert_eq!(status.state, State::Failed);
        assert_eq!(
            status.reason.as_deref(),
            Some(if case == "missing-subtype" {
                "Claude result has no subtype"
            } else {
                "Claude result has no error flag"
            })
        );
        assert_eq!(fs::read(directory.join("output.txt"))?, text.as_bytes());
        assert_eq!(fs::read(directory.join("transcript.jsonl"))?, stdout);
        let collected: Value = record::read_json(&directory.join("result.json"))?;
        assert_eq!(collected["summary"], "retained findings");
        fixture.cleaned()?;
    }
    Ok(())
}
