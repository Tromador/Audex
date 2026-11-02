use super::*;
use std::future::Future;
use std::sync::mpsc;
use std::task::Context as TaskContext;
use std::task::Waker;

#[test]
fn aborted_report_waits_for_stdout_and_preserves_exit_code() -> anyhow::Result<()> {
    const CHILD: &str = "AUDEX_REPORT_OUTPUT_TEST_CHILD";
    let report = format!(
        "# Audex Audit Report\n\nOutcome: aborted_by_operator\n\n{}final line without newline",
        "Retained finding: café — intact.\n".repeat(128)
    );
    if std::env::var_os(CHILD).is_none() {
        let executable = std::env::current_exe()?;
        let output = assert_cmd::Command::new(&executable)
            .args([
                "--exact",
                "audit::report_output_tests::aborted_report_waits_for_stdout_and_preserves_exit_code",
                "--nocapture",
                "--quiet",
            ])
            .env(CHILD, "1")
            .timeout(Duration::from_secs(30))
            .output()?;
        if let Some(root) = std::env::var_os("AUDEX_PROCESS_EVIDENCE_DIR") {
            let root =
                PathBuf::from(root).join(format!("report-output-{}", env!("CARGO_BIN_NAME")));
            std::fs::create_dir_all(&root)?;
            std::fs::write(root.join("stdout"), &output.stdout)?;
            std::fs::write(root.join("stderr"), &output.stderr)?;
            std::fs::write(root.join("expected-report"), &report)?;
            std::fs::write(
                root.join("result.json"),
                serde_json::to_vec_pretty(&serde_json::json!({
                    "executable": executable,
                    "exitCode": output.status.code(),
                    "stdoutBytes": output.stdout.len(),
                    "reportBytes": report.len(),
                }))?,
            )?;
        }
        assert_eq!(
            output.status.code(),
            Some(6),
            "stdout={}, stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        // Only libtest's exact quiet-mode header precedes the production output.
        assert_eq!(
            output.stdout.strip_prefix(b"\nrunning 1 test\n"),
            Some(report.as_bytes())
        );
        assert_eq!(
            output.stderr,
            b"report write remained pending until gate release\n"
        );
        return Ok(());
    }

    let runtime = tokio::runtime::Builder::new_current_thread()
        .max_blocking_threads(1)
        .build()?;
    let code = runtime.block_on(async {
        let (occupied_tx, occupied_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let blocker = tokio::task::spawn_blocking(move || {
            occupied_tx.send(()).expect("announce occupied writer pool");
            release_rx.recv().expect("release writer pool");
        });
        occupied_rx.recv()?;
        // This is the same function used by follow_run, with real Tokio stdout.
        let mut output = Box::pin(write_report(&report));
        let mut context = TaskContext::from_waker(Waker::noop());
        let pending = output.as_mut().poll(&mut context);
        // Release even on assertion failure, so runtime cleanup cannot deadlock.
        release_tx.send(())?;
        assert!(
            pending.is_pending(),
            "production report output completed while its writer was gated: {pending:?}"
        );
        output.await?;
        blocker.await?;
        eprintln!("report write remained pending until gate release");
        let view = AuditRunView {
            collection: Default::default(),
            reviewers: Vec::new(),
            run_id: "run".into(),
            snapshot_id: "snapshot".into(),
            assignment_id: "assignment".into(),
            phase: AuditRunPhase::Cancelled,
            operation_state: AuditOperationState::Unknown,
            observation_pending: true,
            capacity_released: true,
            revision: 9,
            cancellation_requested: true,
            outcome: Some("aborted_by_operator".into()),
            raw_result: None,
            report: Some(report.clone()),
        };
        Ok::<_, anyhow::Error>(terminal_code(&view).expect("published abort exit code"))
    })?;
    std::process::exit(code)
}
