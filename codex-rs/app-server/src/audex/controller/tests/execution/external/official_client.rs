//! Explicit G6 qualification. Run only in the documented offline namespace.
use super::capture::{assert_no_private_files, settled};
use super::*;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;

#[test]
#[ignore = "requires pinned Claude 2.1.314 and the isolated G6 network namespace"]
fn pinned_client_fake_api_and_real_bridge_complete_the_production_path() -> anyhow::Result<()> {
    run_test_with_large_stack("c63-official", || async {
        let binary = std::env::var("AUDEX_CLAUDE_TEST_BIN")?;
        assert_eq!(
            sha256_hex(&std::fs::read(&binary)?),
            "4487042b4c4f5d88d4568f68ae98aae0b1ffae58864cf57fb868641bc5f84594"
        );
        assert_eq!(
            std::fs::read_to_string("/proc/net/route")?.lines().count(),
            1
        );
        let temp = tempfile::tempdir()?;
        let native_server = responses::start_mock_server().await;
        let (controller, threads) = real_fixture(&temp, &native_server.uri()).await;
        let id = seed_external(&controller, &temp, "pinned-official").await?;
        // F2: this integration must exercise the corrected no-machine-lock path.
        std::fs::remove_file(
            controller
                .root
                .join("c71-system-policy/managed-settings.json"),
        )?;
        let store = controller.store().await?;
        let run = store.load_run(&id).await?;
        let runtime = temp.path().join("client-runtime").canonicalize()?;
        let settings: BackendSettings = serde_json::from_value(
            json!({"version":1,"claudeSubscription":{
                "executable":binary,"clientVersion":"2.1.314","profile":temp.path().join("subscription-profile"),"runtimeRoot":runtime
            }}),
        )?;
        std::fs::write(
            controller.root.join("reviewer-backends.json"),
            serde_json::to_vec(&settings)?,
        )?;
        let mut input = RetainedInput::decode(&store.attempt_input(&run.operation_id).await?.0)?;
        input
            .bindings
            .as_mut()
            .unwrap()
            .backend_configuration_identity = settings.identity(&input.route())?;
        let raw = serde_json::to_string(&input)?;
        let digest = sha256_hex(raw.as_bytes());
        sqlx::query(
            "UPDATE audit_attempt_inputs SET request_json=?,input_digest=? WHERE assignment_id=?",
        )
        .bind(raw)
        .bind(&digest)
        .bind(&run.assignment_id)
        .execute(&store.pool_for_tests())
        .await?;
        sqlx::query("UPDATE operations SET input_digest=? WHERE id=?")
            .bind(digest)
            .bind(&run.operation_id)
            .execute(&store.pool_for_tests())
            .await?;
        std::fs::write(
            temp.path().join("fake-api.py"),
            include_str!("../../../../external/fake_api.py"),
        )?;
        std::fs::write(temp.path().join("api-fixture.json"), json!({"result":{"schema_version":1,
            "snapshot_id":run.snapshot_id,"assignment_id":run.assignment_id,"summary":"Pinned official client review","findings":[]}}).to_string())?;
        let mut endpoint = tokio::process::Command::new("/usr/bin/python3")
            .arg(temp.path().join("fake-api.py"))
            .arg(temp.path())
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let ready = receipt(temp.path(), "api-ready.json").await?;
        std::fs::write(
            controller.root.join("c63-fake-api"),
            ready["url"].as_str().unwrap(),
        )?;
        controller.drive(&id).await?;
        if store.submission_boundary(&run.operation_id).await? == SubmissionBoundary::Unsubmitted {
            let reason: Option<String> =
                sqlx::query_scalar("SELECT error FROM operations WHERE id=?")
                    .bind(&run.operation_id)
                    .fetch_one(&store.pool_for_tests())
                    .await?;
            anyhow::bail!("official client refused before prompt: {reason:?}");
        }
        let observed = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                controller.dispatch.reconcile_resources().await?;
                let launch = store.client_launch(&run.operation_id).await?.unwrap();
                if launch.client_accepted_at_ms.is_some() {
                    break anyhow::Ok(launch);
                }
                if let Some(failure) = store.client_capture_failure(&run.assignment_id).await? {
                    use tokio::io::AsyncReadExt;
                    endpoint.kill().await?;
                    endpoint.wait().await?;
                    let mut diagnostics = String::new();
                    if let Some(stderr) = endpoint.stderr.take() { stderr.take(4096).read_to_string(&mut diagnostics).await?; }
                    let receipts = std::fs::read_to_string(temp.path().join("api-receipts.json")).unwrap_or_default();
                    anyhow::bail!("official capture failure before init: {failure}; fake API receipts={receipts}; fixture diagnostics={diagnostics}");
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await??;
        assert!(observed.client_session_id.is_some());
        let child = observed.child_identity().expect("verified official client");
        let expected_socket = runtime
            .join(&observed.execution_id)
            .join(format!("tmp/cc-socks/{}.sock", child.pid));
        assert!(expected_socket.as_os_str().as_bytes().len() <= 103);
        let ownership =
            super::lifecycle_qualification::observed_socket(&controller, &run.operation_id).await?;
        let socket = ownership["sockets"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["path"] == json!(expected_socket))
            .expect("actual pinned client socket must be recorded");
        let metadata = std::fs::symlink_metadata(&expected_socket)?;
        assert_eq!(socket["inode"], metadata.ino());
        assert_eq!(socket["device"], metadata.dev());
        assert_eq!(socket["uid"], metadata.uid());
        assert_eq!(socket["mode"], metadata.mode());
        assert_eq!(socket["owner"], serde_json::to_value(&child)?);
        assert_eq!(socket["disposition"], "observed");
        assert_eq!(ownership["unresolved_socket_paths"], json!([]));
        // The fake API cannot finish until durable socket attribution exists.
        std::fs::write(temp.path().join("socket-observed"), "go")?;
        let terminal = settled(&controller, &id).await?;
        assert_eq!(terminal.assignment_state, AssignmentState::Succeeded);
        assert_eq!(finished(&controller, &id).await["phase"], "completed");
        let requests = receipt(temp.path(), "api-receipts.json").await?;
        assert_eq!(requests.as_array().unwrap().len(), 2);
        assert_eq!(requests[1]["tool_results"].as_array().unwrap().len(), 3);
        assert!(
            store
                .generation_records(&run.assignment_id)
                .await?
                .is_empty()
        );
        let activity = store.activity(&run.assignment_id, 0, 100).await?;
        let public = activity
            .iter()
            .filter(|e| e.kind.starts_with("assistant_"))
            .map(|e| e.payload.as_str())
            .collect::<String>();
        assert_eq!(public.matches("Official progress one.").count(), 1);
        assert_eq!(public.matches("Official progress two.").count(), 1);
        assert_eq!(
            activity.iter().filter(|e| e.kind == "tool_start").count(),
            3
        );
        assert_eq!(
            activity.iter().filter(|e| e.kind == "tool_result").count(),
            3
        );
        let observations: Vec<(String, Option<i64>)> = sqlx::query_as("SELECT message_id,json_extract(usage_json,'$.output_tokens') FROM audit_client_model_observations WHERE assignment_id=? ORDER BY message_id")
            .bind(&run.assignment_id).fetch_all(&store.pool_for_tests()).await?;
        assert_eq!(
            observations,
            vec![
                ("official-msg-1".into(), None),
                ("official-msg-2".into(), None)
            ]
        );
        let usage: String = sqlx::query_scalar(
            "SELECT usage_json FROM audit_client_terminal_evidence WHERE assignment_id=?",
        )
        .bind(&run.assignment_id)
        .fetch_one(&store.pool_for_tests())
        .await?;
        assert_eq!(serde_json::from_str::<Value>(&usage)?["output_tokens"], 78);
        let clean =
            super::lifecycle_qualification::reconcile_until(&controller, &run.operation_id).await?;
        assert!(
            clean["processes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|p| p["role"] == "bridge")
        );
        let removed = clean["sockets"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["path"] == json!(expected_socket))
            .expect("recorded official socket survives reconciliation");
        assert_eq!(removed["inode"], socket["inode"]);
        assert_eq!(removed["device"], socket["device"]);
        assert!(
            removed["disposition"] == "removed_after_local_stop"
                || removed["disposition"] == "absent_after_local_stop"
        );
        let local = store
            .local_execution_view(&run.operation_id)
            .await?
            .unwrap();
        assert!(local.stop_confirmed_at_ms.is_some());
        assert!(local.cleanup_finished_at_ms.is_some());
        assert_eq!(local.status, "local_cleanup_confirmed");
        assert!(!expected_socket.try_exists()?);
        assert_eq!(clean["unresolved_socket_paths"], json!([]));
        println!(
            "D1 pinned client socket: path={}, bytes={}, device={}, inode={}, uid={}, mode={}; durable identity observed before terminal; disposition={}; local stop confirmed; absence verified",
            expected_socket.display(),
            expected_socket.as_os_str().as_bytes().len(),
            metadata.dev(),
            metadata.ino(),
            metadata.uid(),
            metadata.mode(),
            removed["disposition"]
        );
        assert!(
            clean["sockets"]
                .as_array()
                .unwrap()
                .iter()
                .all(|s| s["disposition"] == "removed_after_local_stop"
                    || s["disposition"] == "absent_after_local_stop")
        );
        assert_no_private_files(controller.root.as_path())?;
        assert_no_private_files(&runtime)?;
        endpoint.kill().await?;
        endpoint.wait().await?;
        threads
            .shutdown_all_threads_bounded(Duration::from_secs(5))
            .await;
        println!(
            "C63 pinned official 2.1.314: actual MCP inventory/list/read/search; thinking + multi-record public text; 2 API responses; 2 execution-bound message observations; placeholder output unavailable; aggregate output=78; immutable validated terminal accepted; bridge/group/socket stopped; no external route or genuine credentials"
        );
        Ok(())
    })
}
