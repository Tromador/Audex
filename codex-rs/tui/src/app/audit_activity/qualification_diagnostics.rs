//! Test-only evidence for report retrieval and CLI output; no retries or changed assertions.
use super::support;
use serde_json::json;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::sync::OnceLock;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

pub(super) fn cli(daemon: &support::TestDaemon, args: &[&str], code: i32) -> Output {
    static SEQUENCE: AtomicUsize = AtomicUsize::new(0);
    static IDENTITY: OnceLock<String> = OnceLock::new();
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let root = std::env::var_os("AUDEX_S5_EVIDENCE")
        .map(PathBuf::from)
        .map(|root| root.join(format!("cli-{sequence:03}")));
    let identity = IDENTITY.get_or_init(|| {
        let output = Command::new("sha256sum").arg(&daemon.codex).output();
        format!("{output:?}")
    });
    if code == 6
        && let Some(root) = &root
    {
        snapshot(daemon.home.path(), &root.join("before"));
    }
    let mut command = daemon.command();
    command.arg("audit").args(args);
    let invocation = format!("{command:?}");
    let output = command.output().unwrap();
    if let Some(root) = root {
        let record = json!({
            "sequence": sequence,
            "executable": daemon.codex,
            "executableSha256Command": identity,
            "invocation": invocation,
            "cwd": std::env::current_dir().ok(),
            "codexHome": daemon.home.path(),
            "expectedExitCode": code,
            "status": output.status.to_string(),
            "exitCode": output.status.code(),
            "stdoutBytes": output.stdout.len(),
            "stderrBytes": output.stderr.len(),
            "stdoutSha256": super::digest(&output.stdout),
            "stderrSha256": super::digest(&output.stderr),
        });
        let saved = (|| -> std::io::Result<()> {
            std::fs::create_dir_all(&root)?;
            std::fs::write(root.join("stdout"), &output.stdout)?;
            std::fs::write(root.join("stderr"), &output.stderr)?;
            std::fs::write(root.join("invocation.json"), record.to_string())
        })();
        if let Err(error) = saved {
            eprintln!(
                "CLI diagnostic capture failed at {}: {error}",
                root.display()
            );
        }
        if code == 6 || output.status.code() != Some(code) {
            snapshot(daemon.home.path(), &root.join("after"));
        }
    }
    output
}

// Capture the database through SQLite's read-only backup API, including committed WAL data.
// Copy only report and daemon evidence, never the installed binary or runtime sockets.
fn snapshot(home: &Path, root: &Path) {
    let script = r#"
import hashlib, json, pathlib, shutil, sqlite3, sys
home, root = map(pathlib.Path, sys.argv[1:])
root.mkdir(parents=True, exist_ok=True)
source = sqlite3.connect((home/'audex/audit-state.sqlite3').as_uri()+'?mode=ro', uri=True, timeout=5)
with sqlite3.connect(root/'audit-state.sqlite3') as target:
    source.backup(target)
source.row_factory = sqlite3.Row
state = {name: [dict(row) for row in source.execute('SELECT * FROM '+name)]
         for name in ['audit_runs', 'reports', 'audit_report_selections']}
source.close()
(root/'state.json').write_text(json.dumps(state, indent=2)+'\n')
files = {}
for directory in ['audex/reports', 'app-server-daemon']:
    for path in (home/directory).rglob('*'):
        if path.is_symlink() or not path.is_file():
            continue
        relative = path.relative_to(home)
        destination = root/relative
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(path, destination)
        data = destination.read_bytes()
        files[str(relative)] = {'bytes': len(data), 'sha256': hashlib.sha256(data).hexdigest()}
(root/'files.json').write_text(json.dumps(files, indent=2)+'\n')
"#;
    match Command::new("python3")
        .args(["-c", script])
        .arg(home)
        .arg(root)
        .output()
    {
        Ok(output) if output.status.success() => {}
        output => eprintln!("Report diagnostic snapshot {}: {output:?}", root.display()),
    }
}
