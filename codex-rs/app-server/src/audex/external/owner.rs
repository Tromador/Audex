//! The spawning thread lasts for the process lifetime, including when Tokio workers retire.
use codex_app_server_daemon::{LinuxChildIdentity, read_linux_child_identity};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Arc;
use std::sync::{OnceLock, mpsc};
use std::time::Duration;

pub(super) struct SpawnSpec {
    pub command: Command,
    pub nonce: String,
    pub nonce_argument: String,
    pub executable_digest: String,
    pub binding_argument: String,
}

pub(super) struct OwnedChild {
    pub identity: LinuxChildIdentity,
    nonce: String,
    sender: mpsc::Sender<Message>,
}

impl OwnedChild {
    pub(super) fn interrupt(&self) -> Arc<dyn Fn() + Send + Sync> {
        let sender = self.sender.clone();
        let nonce = self.nonce.clone();
        Arc::new(move || {
            let _ = sender.send(Message::Stop(nonce.clone()));
        })
    }
}

impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.sender.send(Message::Stop(self.nonce.clone()));
    }
}

pub(super) struct Spawned {
    pub owned: OwnedChild,
    pub stdin: ChildStdin,
    pub stdout: ChildStdout,
    pub stderr: ChildStderr,
}

enum Message {
    Spawn(
        SpawnSpec,
        tokio::sync::oneshot::Sender<anyhow::Result<Spawned>>,
    ),
    Stop(String),
    Evidence(
        String,
        tokio::sync::oneshot::Sender<Option<super::lifecycle::OwnershipEvidence>>,
    ),
    Forget(String),
}

struct Running {
    child: Option<Child>,
    identity: LinuxChildIdentity,
    nonce_argument: String,
    binding_argument: String,
    evidence: super::lifecycle::OwnershipEvidence,
    stopping: bool,
}

impl Running {
    fn observe(&mut self) {
        let directory = std::path::Path::new(&self.nonce_argument)
            .parent()
            .expect("private runtime");
        let observed = super::lifecycle::observe(
            Some(&self.identity),
            &self.nonce_argument,
            &self.binding_argument,
            directory,
            &self.evidence,
        );
        self.evidence = observed.evidence;
    }

    fn stop(&mut self) {
        self.observe();
        self.stopping = true;
        if let Some(child) = &mut self.child {
            // SAFETY: this direct Child has never been reaped. Its PID reserves
            // the original group number even if the leader is now a zombie.
            // Cold reconstruction never uses this numeric-group operation.
            unsafe {
                libc::kill(-(self.identity.process_group as i32), libc::SIGKILL);
            }
            let _ = child.kill();
        }
        for member in &self.evidence.processes {
            let _ = super::lifecycle::signal_verified(&member.identity);
        }
    }

    fn poll(&mut self) {
        if self.child.is_none() {
            return;
        }
        self.observe();
        if self.stopping
            || matches!(
                codex_app_server_daemon::read_linux_process_identity(self.identity.pid),
                Ok(None)
            )
        {
            // Terminate the group before reaping its leader, including normal exit.
            self.stop();
            if self
                .child
                .as_mut()
                .is_some_and(|child| matches!(child.try_wait(), Ok(Some(_))))
            {
                self.child = None;
            }
        }
    }
}

fn sender() -> &'static mpsc::Sender<Message> {
    static OWNER: OnceLock<mpsc::Sender<Message>> = OnceLock::new();
    OWNER.get_or_init(|| {
        let (sender, receiver) = mpsc::channel();
        let response_sender = sender.clone();
        std::thread::Builder::new()
            .name("audex-client-owner".into())
            .spawn(move || {
                let mut children: HashMap<String, Running> = HashMap::new();
                let mut next_poll = std::time::Instant::now();
                loop {
                    match receiver.recv_timeout(Duration::from_millis(20)) {
                        Ok(Message::Spawn(spec, reply)) => {
                            if reply.is_closed() {
                                continue;
                            }
                            let result = spawn(spec, &response_sender, &mut children);
                            let _ = reply.send(result);
                        }
                        Ok(Message::Stop(nonce)) => {
                            if let Some(running) = children.get_mut(&nonce) {
                                running.stop();
                            }
                        }
                        Ok(Message::Evidence(nonce, reply)) => {
                            let _ = reply.send(children.get_mut(&nonce).map(|running| {
                                running.poll();
                                running.evidence.clone()
                            }));
                        }
                        Ok(Message::Forget(nonce)) => {
                            if children
                                .get(&nonce)
                                .is_some_and(|running| running.child.is_none())
                            {
                                children.remove(&nonce);
                            }
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                    if std::time::Instant::now() >= next_poll {
                        for running in children.values_mut() {
                            running.poll();
                        }
                        next_poll = std::time::Instant::now() + Duration::from_millis(100);
                    }
                }
            })
            .expect("start Audex client owner thread");
        sender
    })
}

fn spawn(
    mut spec: SpawnSpec,
    sender: &mpsc::Sender<Message>,
    children: &mut HashMap<String, Running>,
) -> anyhow::Result<Spawned> {
    anyhow::ensure!(!children.contains_key(&spec.nonce), "launch already owned");
    verify_version(&spec)?;
    anyhow::ensure!(
        super::super::snapshot::manifest::sha256_hex(&std::fs::read(spec.command.get_program())?)
            == spec.executable_digest,
        "client executable changed during qualification"
    );
    let expected_parent = std::process::id() as libc::pid_t;
    spec.command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // SAFETY: only async-signal-safe Linux syscalls execute between fork and exec.
    unsafe {
        spec.command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0
                || libc::getppid() != expected_parent
                || libc::setpgid(0, 0) != 0
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = spec.command.spawn()?;
    let identity = match read_linux_child_identity(child.id(), &spec.nonce_argument) {
        Ok(identity)
            if identity.parent_pid == expected_parent as u32
                && identity.process_group == child.id() =>
        {
            identity
        }
        _ => {
            // The direct, unreaped Child handle is ownership proof even if exec
            // exited too quickly to obtain durable /proc evidence. Never deliver input.
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("child identity unavailable");
        }
    };
    let stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let fds = [stdin.as_raw_fd(), stdout.as_raw_fd(), stderr.as_raw_fd()];
    // SAFETY: fd is this child's live pipe; flags do not change its ownership.
    if fds
        .into_iter()
        .any(|fd| unsafe { libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK) } == -1)
    {
        let _ = child.kill();
        let _ = child.wait();
        return Err(std::io::Error::last_os_error().into());
    }
    children.insert(
        spec.nonce.clone(),
        Running {
            child: Some(child),
            identity: identity.clone(),
            nonce_argument: spec.nonce_argument,
            binding_argument: spec.binding_argument,
            evidence: super::lifecycle::OwnershipEvidence::default(),
            stopping: false,
        },
    );
    Ok(Spawned {
        owned: OwnedChild {
            identity,
            nonce: spec.nonce,
            sender: sender.clone(),
        },
        stdin,
        stdout,
        stderr,
    })
}

pub(super) fn stop(nonce: &str) {
    let _ = sender().send(Message::Stop(nonce.into()));
}

pub(super) fn forget(nonce: &str) {
    let _ = sender().send(Message::Forget(nonce.into()));
}

pub(super) async fn evidence(nonce: &str) -> Option<super::lifecycle::OwnershipEvidence> {
    let (reply, result) = tokio::sync::oneshot::channel();
    sender().send(Message::Evidence(nonce.into(), reply)).ok()?;
    tokio::time::timeout(Duration::from_secs(1), result)
        .await
        .ok()?
        .ok()?
}

pub(super) async fn launch(spec: SpawnSpec) -> anyhow::Result<Spawned> {
    let (reply, result) = tokio::sync::oneshot::channel();
    sender()
        .send(Message::Spawn(spec, reply))
        .map_err(|_| anyhow::anyhow!("client owner unavailable"))?;
    result.await?
}

/// One logical submission, including partial pipe writes. No error is retryable.
pub(super) async fn write_prompt(stdin: ChildStdin, prompt: &[u8]) -> anyhow::Result<()> {
    let mut pipe = tokio::io::unix::AsyncFd::new(stdin)?;
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut offset = 0;
        while offset < prompt.len() {
            let mut ready = pipe.writable_mut().await?;
            match ready.try_io(|pipe| pipe.get_mut().write(&prompt[offset..])) {
                Ok(Ok(0)) => anyhow::bail!("closed prompt input"),
                Ok(Ok(n)) => offset += n,
                Ok(Err(error)) => return Err(error.into()),
                Err(_) => {}
            }
        }
        Ok(())
    })
    .await?
}

// A bounded, no-prompt identity probe on the same lifetime owner and isolated env.
fn verify_version(spec: &SpawnSpec) -> anyhow::Result<()> {
    let mut command = Command::new(spec.command.get_program());
    command.arg("--version").env_clear();
    for (key, value) in spec.command.get_envs() {
        if let Some(value) = value {
            command.env(key, value);
        }
    }
    if let Some(cwd) = spec.command.get_current_dir() {
        command.current_dir(cwd);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let expected = std::process::id() as libc::pid_t;
    // SAFETY: async-signal-safe parent ownership syscalls only.
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0
                || libc::getppid() != expected
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn()?;
    let mut stdout = child.stdout.take().expect("version stdout");
    // SAFETY: the live pipe is exclusively owned here.
    unsafe {
        libc::fcntl(stdout.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK);
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10))
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                anyhow::bail!("client version probe unavailable");
            }
        }
    };
    let mut bytes = [0u8; 1024];
    let n = stdout.read(&mut bytes)?;
    anyhow::ensure!(
        status.success()
            && std::str::from_utf8(&bytes[..n])?.trim()
                == format!("{} (Claude Code)", super::config::CLIENT_VERSION),
        "unqualified executable version"
    );
    Ok(())
}
