//! Windows activation on WSL, qualified against 2.1.314's V9t/dhr/ke/Ui/Dd.
//! https://code.claude.com/docs/en/settings-reference#wslinheritswindowssettings
//! Inspection tools and directories are not policy. Only the admin sources can
//! opt in; malformed/unreadable admin documents are a separate blocking case.
use super::*;
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const HKLM: &str = r"HKLM\SOFTWARE\Policies\ClaudeCode";
const HKCU: &str = r"HKCU\SOFTWARE\Policies\ClaudeCode";
const MISSING_VALUE: &str = "unable to find the specified registry key or value";

pub(super) struct Selection {
    pub settings: Option<Value>,
    pub registry: bool,
    pub inherits: bool,
}

pub(super) fn inspect(
    kernel: &str,
    registry: &Path,
    directory: &Path,
    evidence: &mut Vec<Value>,
) -> anyhow::Result<Selection> {
    let kernel = kernel.to_ascii_lowercase();
    let wsl = kernel.contains("microsoft") || kernel.contains("wsl");
    evidence.push(json!({"wsl":wsl}));
    let linux = Selection {
        settings: None,
        registry: false,
        inherits: false,
    };
    if !wsl {
        return Ok(linux);
    }
    let hklm = read_registry(registry, HKLM, evidence)?;
    let documents = read_documents(directory, evidence)?;
    let mut enabled = inheritance(
        hklm.as_ref()
            .and_then(|v| v.get("wslInheritsWindowsSettings")),
    )?;
    // Ui scans for any true: a later false drop-in does not cancel an opt-in.
    // Inspect every document even after true so unreadable/malformed authorities
    // cannot be hidden by an earlier source or by a disabled inheritance flag.
    for document in &documents {
        enabled |= inheritance(document.get("wslInheritsWindowsSettings"))?;
    }
    evidence.push(json!({"windowsInheritance":enabled}));
    if !enabled {
        return Ok(linux);
    }
    anyhow::ensure!(
        metadata_if_present(&directory.join("managed-mcp.json"))?.is_none(),
        "Windows managed MCP replaces the source-only inventory"
    );
    let mut files = json!({});
    let mut file_policy = false;
    for document in &documents {
        validate_document(document)?;
        file_policy |= has_policy(document);
        merge(&mut files, document);
    }
    if let Some(hklm) = hklm {
        validate_document(&hklm)?;
        if has_policy(&hklm) {
            return Ok(Selection {
                settings: Some(hklm),
                registry: true,
                inherits: true,
            });
        }
    }
    // A flag-only Windows document leaves Linux eligible. Any other admin
    // content suppresses Linux. HKCU is checked only if Linux also has no admin
    // content; parent settings are excluded by the qualified launch.
    Ok(Selection {
        settings: file_policy.then_some(files),
        registry: false,
        inherits: true,
    })
}

pub(super) fn user_policy(
    registry: &Path,
    evidence: &mut Vec<Value>,
) -> anyhow::Result<Option<Value>> {
    let Some(mut value) = read_registry(registry, HKCU, evidence)? else {
        return Ok(None);
    };
    if !inheritance(value.get("wslInheritsWindowsSettings"))? {
        return Ok(None);
    }
    validate_document(&value)?;
    // Pinned ke() removes these selectors from the eligible HKCU payload.
    let object = value.as_object_mut().expect("validated object");
    object.remove("wslInheritsWindowsSettings");
    object.remove("managedSourcesBehavior");
    Ok(Some(value))
}

pub(super) fn inheritance(value: Option<&Value>) -> anyhow::Result<bool> {
    match value {
        None | Some(Value::Null | Value::Bool(false)) => Ok(false),
        Some(Value::Bool(true)) => Ok(true),
        Some(Value::String(value)) if value == "true" => Ok(true),
        Some(Value::String(value)) if value == "false" => Ok(false),
        _ => anyhow::bail!("Windows inheritance flag is indeterminate"),
    }
}

pub(super) fn has_policy(document: &Value) -> bool {
    document
        .as_object()
        .expect("validated object")
        .iter()
        .any(|(key, value)| {
            !matches!(
                key.as_str(),
                "wslInheritsWindowsSettings" | "managedSourcesBehavior"
            ) && !value.is_null()
        })
}

fn read_documents(directory: &Path, evidence: &mut Vec<Value>) -> anyhow::Result<Vec<Value>> {
    let mut documents = Vec::new();
    if let Some(metadata) = metadata_if_present(directory)? {
        anyhow::ensure!(
            metadata.is_dir(),
            "Windows policy directory is not a directory"
        );
    }
    if let Some(value) = read_document(&directory.join("managed-settings.json"), evidence)? {
        documents.push(value);
    }
    let drop_ins = directory.join("managed-settings.d");
    if let Some(metadata) = metadata_if_present(&drop_ins)? {
        anyhow::ensure!(metadata.is_dir(), "Windows drop-in directory is unreadable");
        let mut paths = Vec::new();
        for entry in std::fs::read_dir(&drop_ins)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("invalid policy filename"))?;
            let kind = entry.file_type()?;
            if !name.starts_with('.')
                && name.ends_with(".json")
                && (kind.is_file() || kind.is_symlink())
            {
                anyhow::ensure!(name.is_ascii(), "unqualified drop-in ordering");
                paths.push(entry.path());
                anyhow::ensure!(paths.len() <= MAX_DROP_INS, "too many managed drop-ins");
            }
        }
        paths.sort();
        for path in paths {
            documents.push(
                read_document(&path, evidence)?
                    .ok_or_else(|| anyhow::anyhow!("Windows managed drop-in disappeared"))?,
            );
        }
    }
    Ok(documents)
}

fn read_document(path: &Path, evidence: &mut Vec<Value>) -> anyhow::Result<Option<Value>> {
    let Some(metadata) = metadata_if_present(path)? else {
        evidence.push(json!({"path":path,"absent":true}));
        return Ok(None);
    };
    // These fixed Windows admin paths use Windows ACLs. DrvFS's synthetic Unix
    // write bits do not establish Windows policy authority or unreadability.
    // Linked/non-regular sources remain outside the supported qualification.
    anyhow::ensure!(
        metadata.is_file() && metadata.len() <= MAX_FILE,
        "unqualified Windows policy file"
    );
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let opened = file.metadata()?;
    anyhow::ensure!(
        metadata.dev() == opened.dev() && metadata.ino() == opened.ino(),
        "Windows policy changed while opening"
    );
    let mut bytes = Vec::new();
    file.take(MAX_FILE + 1).read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() as u64 <= MAX_FILE,
        "Windows policy file exceeds bound"
    );
    // Ui treats an empty/whitespace Windows file as an empty document.
    let value = if bytes.iter().all(u8::is_ascii_whitespace) {
        json!({})
    } else {
        parse_document(&bytes)?
    };
    evidence.push(
        json!({"path":path,"digest":sha256_hex(&bytes),"device":opened.dev(),"inode":opened.ino()}),
    );
    Ok(Some(value))
}

fn parse_document(bytes: &[u8]) -> anyhow::Result<Value> {
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|_| anyhow::anyhow!("Windows policy is not valid JSON"))?;
    anyhow::ensure!(value.is_object(), "Windows policy is not an object");
    Ok(value)
}

fn read_registry(
    path: &Path,
    hive: &str,
    evidence: &mut Vec<Value>,
) -> anyhow::Result<Option<Value>> {
    let Some(metadata) = metadata_if_present(path)? else {
        // The pinned client uses this exact executable, with no PATH fallback.
        evidence.push(json!({"registryTool":path,"absent":true}));
        return Ok(None);
    };
    anyhow::ensure!(metadata.is_file(), "Windows registry tool is indeterminate");
    let result = query(path, hive, Duration::from_secs(5))?;
    let value = registry_document(&result)?;
    evidence.push(json!({"registry":hive,"value":"Settings","digest":sha256_hex(&result.stdout),"absent":value.is_none()}));
    Ok(value)
}

fn registry_document(result: &Output) -> anyhow::Result<Option<Value>> {
    if !result.status.success() {
        // Match 2.1.314's explicit absence response. Access denied, unknown or
        // localised diagnostics, timeout and failed interop are not absence.
        anyhow::ensure!(
            result.status.code() == Some(1)
                && result.stdout.is_empty()
                && std::str::from_utf8(&result.stderr)?.contains(MISSING_VALUE),
            "Windows registry policy could not be established"
        );
        return Ok(None);
    }
    let stdout = std::str::from_utf8(&result.stdout)?;
    // The pinned parser accepts an indented Settings REG_SZ/REG_EXPAND_SZ
    // record, case-insensitively, with the remainder as a possibly multiline
    // JSON string. Parse that fixed format without another dependency.
    let mut offset = 0;
    for line in stdout.split_inclusive('\n') {
        let rest = &stdout[offset..];
        offset += line.len();
        if !line.starts_with([' ', '\t']) {
            continue;
        }
        let Some((name, rest)) = rest.trim_start_matches([' ', '\t']).split_once([' ', '\t'])
        else {
            continue;
        };
        if !name.eq_ignore_ascii_case("Settings") {
            continue;
        }
        let (kind, document) = rest
            .trim_start_matches([' ', '\t'])
            .split_once([' ', '\t'])
            .ok_or_else(|| anyhow::anyhow!("Windows registry Settings value is not a string"))?;
        anyhow::ensure!(
            kind.eq_ignore_ascii_case("REG_SZ") || kind.eq_ignore_ascii_case("REG_EXPAND_SZ"),
            "Windows registry Settings value is not a string"
        );
        return Ok(Some(parse_document(document.trim().as_bytes())?));
    }
    anyhow::bail!("Windows registry Settings value is not a string")
}

struct QueryChild(Child);
impl Drop for QueryChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn query(path: &Path, hive: &str, timeout: Duration) -> anyhow::Result<Output> {
    let mut command = Command::new(path);
    // Exactly dhr's query/view; no shell, alternate executable, auth or policy writes.
    command
        .args(["query", hive, "/v", "Settings"])
        .env_clear()
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let parent = std::process::id() as libc::pid_t;
    // SAFETY: only async-signal-safe ownership syscalls run after fork.
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 || libc::getppid() != parent
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = QueryChild(
        command
            .spawn()
            .map_err(|_| anyhow::anyhow!("Windows registry query unavailable"))?,
    );
    let mut stdout = child.0.stdout.take().expect("query stdout");
    let mut stderr = child.0.stderr.take().expect("query stderr");
    for fd in [stdout.as_raw_fd(), stderr.as_raw_fd()] {
        // SAFETY: these live pipes are exclusively owned here.
        anyhow::ensure!(
            unsafe { libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK) } != -1,
            "Windows registry query pipe unavailable"
        );
    }
    let deadline = Instant::now() + timeout;
    let mut out = Vec::new();
    let mut err = Vec::new();
    loop {
        let out_closed = drain(&mut stdout, &mut out)?;
        let err_closed = drain(&mut stderr, &mut err)?;
        if let Some(status) = child.0.try_wait()? {
            if out_closed && err_closed {
                return Ok(Output {
                    status,
                    stdout: out,
                    stderr: err,
                });
            }
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "Windows registry query timed out"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn drain(pipe: &mut impl Read, bytes: &mut Vec<u8>) -> anyhow::Result<bool> {
    loop {
        let mut chunk = [0; 8192];
        match pipe.read(&mut chunk) {
            Ok(0) => return Ok(true),
            Ok(n) => {
                anyhow::ensure!(
                    bytes.len() + n <= MAX_FILE as usize,
                    "Windows registry response exceeds bound"
                );
                bytes.extend_from_slice(&chunk[..n]);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(false),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => anyhow::bail!("Windows registry response unreadable"),
        }
    }
}

#[cfg(test)]
mod tests;
