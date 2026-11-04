//! Verified local resource observations. This module has no workflow authority.
use codex_app_server_daemon::{
    LinuxChildIdentity, read_linux_child_identity, read_linux_process_identity,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};

const MAX_MEMBERS: usize = 128;
const MAX_SOCKETS: usize = 32;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct ProcessEvidence {
    pub identity: LinuxChildIdentity,
    pub role: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct SocketEvidence {
    pub path: PathBuf,
    pub device: u64,
    pub inode: u64,
    pub uid: u32,
    pub mode: u32,
    pub changed_seconds: i64,
    pub changed_nanoseconds: i64,
    pub owner: LinuxChildIdentity,
    pub disposition: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct OwnershipEvidence {
    pub processes: Vec<ProcessEvidence>,
    pub sockets: Vec<SocketEvidence>,
    // Observed through an owned process descriptor, but outside the private
    // runtime. These paths are obligations, never authority to remove a file.
    pub unresolved_socket_paths: Vec<PathBuf>,
}

pub(super) struct LocalObservation {
    pub evidence: OwnershipEvidence,
    pub live: Vec<ProcessEvidence>,
    pub uncertain: bool,
    pub reason: &'static str,
}

pub(super) fn same_process(a: &LinuxChildIdentity, b: &LinuxChildIdentity) -> bool {
    a.pid == b.pid && a.start_ticks == b.start_ticks && a.boot_id == b.boot_id
}

fn disappeared(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<std::io::Error>()
        .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound)
}

/// pidfd pins the exact process across the final identity check and signal.
/// Cold reconstruction never sends a signal to a numeric PID or PGID alone.
pub(super) fn signal_verified(identity: &LinuxChildIdentity) -> anyhow::Result<()> {
    // SAFETY: pidfd_open takes scalar arguments and returns an owned descriptor.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, identity.pid, 0) };
    if fd < 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            return Ok(());
        }
        return Err(error.into());
    }
    // SAFETY: a successful pidfd_open returns a fresh descriptor owned here.
    let fd = unsafe { OwnedFd::from_raw_fd(fd as i32) };
    let Some(actual) = read_linux_process_identity(identity.pid)? else {
        return Ok(());
    };
    anyhow::ensure!(
        same_process(&actual, identity),
        "process identity changed before signal"
    );
    // SAFETY: the pidfd, not a reusable PID, selects the target; no siginfo pointer.
    if unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            fd.as_raw_fd(),
            libc::SIGKILL,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    } < 0
    {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(error.into());
        }
    }
    Ok(())
}

fn socket_metadata(path: &Path, owner: &LinuxChildIdentity) -> anyhow::Result<SocketEvidence> {
    let metadata = std::fs::symlink_metadata(path)?;
    anyhow::ensure!(
        metadata.file_type().is_socket() && metadata.uid() == unsafe { libc::geteuid() },
        "socket ownership unavailable"
    );
    anyhow::ensure!(
        path.is_absolute()
            && path
                .parent()
                .is_some_and(|p| p.canonicalize().is_ok_and(|actual| actual == p)),
        "socket parent identity unavailable"
    );
    Ok(SocketEvidence {
        path: path.into(),
        device: metadata.dev(),
        inode: metadata.ino(),
        uid: metadata.uid(),
        mode: metadata.mode(),
        changed_seconds: metadata.ctime(),
        changed_nanoseconds: metadata.ctime_nsec(),
        owner: owner.clone(),
        disposition: "observed".into(),
    })
}

fn same_socket(a: &SocketEvidence, b: &SocketEvidence) -> bool {
    a.path == b.path
        && a.device == b.device
        && a.inode == b.inode
        && a.uid == b.uid
        && a.mode == b.mode
        && a.changed_seconds == b.changed_seconds
        && a.changed_nanoseconds == b.changed_nanoseconds
}

fn sockets_for(
    identity: &LinuxChildIdentity,
    directory: &Path,
    unresolved: &mut Vec<PathBuf>,
) -> anyhow::Result<Vec<SocketEvidence>> {
    let unix = std::fs::read_to_string("/proc/net/unix")?;
    let mut paths = HashMap::new();
    let mut path_counts = HashMap::new();
    for line in unix.lines().skip(1) {
        let columns = line.split_whitespace().take(7).collect::<Vec<_>>();
        if columns.len() == 7
            && let Some(start) = line.find('/')
        {
            let path = &line[start..];
            paths.insert(format!("socket:[{}]", columns[6]), PathBuf::from(path));
            *path_counts.entry(path.to_owned()).or_insert(0usize) += 1;
        }
    }
    let mut result = Vec::new();
    let mut vfs = None;
    for entry in std::fs::read_dir(format!("/proc/{}/fd", identity.pid))? {
        let entry = entry?;
        let target = match std::fs::read_link(entry.path()) {
            Ok(target) => target,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        let Some(path) = paths.get(target.to_string_lossy().as_ref()) else {
            continue;
        };
        if !path.starts_with(directory) && !unresolved.contains(path) {
            // Retain the observation even if the path has disappeared or now
            // names a foreign inode. Process exit cannot discharge this debt.
            anyhow::ensure!(
                unresolved.len() < MAX_SOCKETS,
                "socket observation bound exhausted"
            );
            unresolved.push(path.clone());
        }
        anyhow::ensure!(
            path.starts_with(directory) && path_counts.get(&path.display().to_string()) == Some(&1),
            "unqualified or replaced client socket"
        );
        match socket_metadata(path, identity) {
            Ok(socket) => {
                if vfs.is_none() {
                    vfs = Some(super::socket_identity::filesystem_identities()?);
                }
                let kernel_inode: u32 = target
                    .to_string_lossy()
                    .trim_start_matches("socket:[")
                    .trim_end_matches(']')
                    .parse()?;
                let expected = (
                    u32::try_from(socket.inode)?,
                    super::socket_identity::encoded_device(socket.device),
                );
                anyhow::ensure!(
                    vfs.as_ref().and_then(|v| v.get(&kernel_inode)) == Some(&expected),
                    "socket path no longer names its bound inode"
                );
                result.push(socket);
            }
            Err(e) if disappeared(&e) => {}
            Err(e) => return Err(e),
        }
    }
    anyhow::ensure!(
        read_linux_process_identity(identity.pid)?
            .is_some_and(|actual| same_process(&actual, identity)),
        "socket owner changed during inspection"
    );
    Ok(result)
}

/// Observe nonce-bearing client/bridge processes, verified group members and
/// previously recorded identities. Missing/reused identities never imply replay.
pub(super) fn observe(
    expected: Option<&LinuxChildIdentity>,
    nonce_argument: &str,
    binding_argument: &str,
    directory: &Path,
    retained: &OwnershipEvidence,
) -> LocalObservation {
    let mut result = LocalObservation {
        evidence: retained.clone(),
        live: Vec::new(),
        uncertain: false,
        reason: "local_processes_absent",
    };
    #[cfg(test)]
    if directory.join("c63-identity-unreadable").exists() {
        result.uncertain = true;
        result.reason = "local_identity_unconfirmed";
        return result;
    }
    let mut live = HashMap::new();
    for known in &retained.processes {
        match read_linux_process_identity(known.identity.pid) {
            Ok(Some(actual)) if same_process(&actual, &known.identity) => {
                live.insert(actual.pid, known.clone());
            }
            Ok(_) => {}
            Err(e) if disappeared(&e) => {}
            Err(_) => result.uncertain = true,
        }
    }
    let mut client = None;
    if let Some(expected) = expected {
        match read_linux_process_identity(expected.pid) {
            Ok(Some(actual)) if same_process(&actual, expected) => {
                match read_linux_child_identity(expected.pid, nonce_argument) {
                    Ok(actual) if actual.process_group == expected.process_group => {
                        client = Some(actual.clone());
                        live.insert(
                            actual.pid,
                            ProcessEvidence {
                                identity: actual,
                                role: "client".into(),
                            },
                        );
                    }
                    _ => result.uncertain = true,
                }
            }
            Ok(_) => {}
            Err(e) if disappeared(&e) => {}
            Err(_) => result.uncertain = true,
        }
    }
    let mut candidates = Vec::new();
    let scan = (|| -> anyhow::Result<()> {
        for entry in std::fs::read_dir("/proc")? {
            let entry = entry?;
            let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
                continue;
            };
            let metadata = match entry.metadata() {
                Ok(metadata) => metadata,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            };
            if metadata.uid() != unsafe { libc::geteuid() } {
                continue;
            }
            let actual = match read_linux_process_identity(pid) {
                Ok(Some(actual)) => actual,
                Ok(None) => continue,
                Err(e) if disappeared(&e) => continue,
                Err(e) => return Err(e),
            };
            let argv = match std::fs::read(entry.path().join("cmdline")) {
                Ok(argv) => argv,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            };
            let args = argv.split(|b| *b == 0).collect::<Vec<_>>();
            let is_client = args.contains(&nonce_argument.as_bytes());
            let is_bridge = !binding_argument.is_empty()
                && args
                    .windows(2)
                    .any(|a| a[0] == b"--binding" && a[1] == binding_argument.as_bytes())
                && args.contains(&b"audit-source-mcp".as_slice());
            if is_client {
                if expected.is_none_or(|expected| !same_process(expected, &actual))
                    || actual.process_group != actual.pid
                {
                    result.uncertain = true;
                    continue;
                }
                client = Some(actual.clone());
                live.insert(
                    pid,
                    ProcessEvidence {
                        identity: actual.clone(),
                        role: "client".into(),
                    },
                );
            }
            candidates.push((actual, is_bridge));
        }
        Ok(())
    })();
    if scan.is_err() {
        result.uncertain = true;
    }
    let group = client.as_ref().or(expected).map(|i| i.process_group);
    for (actual, bridge) in candidates {
        let verified_group = client
            .as_ref()
            .is_some_and(|leader| actual.process_group == leader.process_group);
        if client.is_none()
            && group == Some(actual.process_group)
            && !bridge
            && !retained
                .processes
                .iter()
                .any(|p| same_process(&p.identity, &actual))
        {
            result.uncertain = true;
        }
        if bridge && group != Some(actual.process_group) {
            result.uncertain = true;
        } else if bridge || verified_group {
            live.entry(actual.pid).or_insert(ProcessEvidence {
                identity: actual,
                role: if bridge { "bridge" } else { "group_member" }.into(),
            });
        }
    }
    if client.is_none() && expected.is_some() {
        // Known members remain independently signalable by pidfd. An unbound
        // numeric group alone is never authority to signal additional processes.
        if let Some(group) = group {
            for known in live.values().filter(|p| p.identity.process_group == group) {
                if !retained
                    .processes
                    .iter()
                    .any(|p| same_process(&p.identity, &known.identity))
                    && known.role != "bridge"
                {
                    result.uncertain = true;
                }
            }
        }
    }
    for process in live.values() {
        if !result
            .evidence
            .processes
            .iter()
            .any(|p| same_process(&p.identity, &process.identity))
        {
            if result.evidence.processes.len() == MAX_MEMBERS {
                result.uncertain = true;
                break;
            }
            result.evidence.processes.push(process.clone());
        }
        match sockets_for(
            &process.identity,
            directory,
            &mut result.evidence.unresolved_socket_paths,
        ) {
            Ok(sockets) => {
                for socket in sockets {
                    if !result
                        .evidence
                        .sockets
                        .iter()
                        .any(|old| old.path == socket.path)
                    {
                        if result.evidence.sockets.len() == MAX_SOCKETS {
                            result.uncertain = true;
                            break;
                        }
                        result.evidence.sockets.push(socket);
                    }
                }
            }
            Err(e) if disappeared(&e) => {}
            Err(_) => result.uncertain = true,
        }
    }
    result.live = live.into_values().collect();
    result.reason = if result.uncertain {
        "local_identity_unconfirmed"
    } else if result.live.is_empty() {
        "local_processes_absent"
    } else {
        "local_processes_live"
    };
    result
}

/// Called only after confirmed local death. Replaced/foreign sockets are retained.
pub(super) fn clean_sockets(
    evidence: &mut OwnershipEvidence,
    directory: &Path,
) -> anyhow::Result<bool> {
    let mut complete = evidence.unresolved_socket_paths.is_empty();
    let recorded = evidence
        .sockets
        .iter()
        .map(|s| s.path.clone())
        .collect::<HashSet<_>>();
    for socket in &mut evidence.sockets {
        match socket_metadata(&socket.path, &socket.owner) {
            Ok(actual) if same_socket(&actual, socket) => {
                std::fs::remove_file(&socket.path)?;
                socket.disposition = "removed_after_local_stop".into();
            }
            Ok(_) => socket.disposition = "replacement_preserved".into(),
            Err(e) if disappeared(&e) => socket.disposition = "absent_after_local_stop".into(),
            Err(_) => {
                socket.disposition = "identity_unconfirmed".into();
                complete = false;
            }
        }
    }
    // Unrecorded socket remnants are never attributed from their name or PID.
    // Enumerating the private runtime detects the obligation but cannot authorise deletion.
    let mut pending = vec![directory.to_path_buf()];
    let mut entries = 0;
    while let Some(path) = pending.pop() {
        let directory = match std::fs::read_dir(path) {
            Ok(directory) => directory,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        for entry in directory {
            let entry = entry?;
            entries += 1;
            anyhow::ensure!(entries <= 4096, "runtime inspection bound exhausted");
            let kind = entry.file_type()?;
            if kind.is_dir() {
                pending.push(entry.path());
            } else if kind.is_socket() && !recorded.contains(&entry.path()) {
                complete = false;
            }
        }
    }
    Ok(complete)
}
