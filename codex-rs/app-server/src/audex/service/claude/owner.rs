//! A dedicated spawning thread owns each child through group cleanup and reaping.
use super::launch::Launch;
use anyhow::{Context, Result};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use tokio::io::unix::AsyncFd;
use tokio::sync::oneshot;

const TERM_GRACE: Duration = Duration::from_millis(500);

pub(super) struct Owner {
    stop: mpsc::Sender<()>,
    pub exit: oneshot::Receiver<std::io::Result<ExitStatus>>,
}
impl Owner {
    pub fn stop(&self) {
        let _ = self.stop.send(());
    }
}
impl Drop for Owner {
    fn drop(&mut self) {
        self.stop();
    }
}

pub(super) struct Spawned {
    pub owner: Owner,
    pub stdin: ChildStdin,
    pub stdout: ChildStdout,
    pub stderr: ChildStderr,
}

pub(super) async fn spawn(launch: Launch) -> Result<(Spawned, Vec<u8>)> {
    let Launch {
        mut command,
        prompt,
        runtime,
    } = launch;
    let (ready, started) = oneshot::channel();
    let (stop, stopping) = mpsc::channel();
    let (finished, exit) = oneshot::channel();
    std::thread::Builder::new()
        .name("audex-claude-owner".into())
        .spawn(move || {
            // Keep the runtime and spawning thread alive until the child is reaped.
            let _runtime = runtime;
            let expected_parent = std::process::id() as libc::pid_t;
            command
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            // SAFETY: only async-signal-safe syscalls between fork and exec.
            unsafe {
                command.pre_exec(move || {
                    if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0
                        || libc::getppid() != expected_parent
                        || libc::setpgid(0, 0) != 0
                    {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            let mut child = match command.spawn() {
                Ok(child) => child,
                Err(error) => {
                    let _ = ready.send(Err(error));
                    return;
                }
            };
            let stdin = child.stdin.take().expect("piped stdin");
            let stdout = child.stdout.take().expect("piped stdout");
            let stderr = child.stderr.take().expect("piped stderr");
            for fd in [stdin.as_raw_fd(), stdout.as_raw_fd(), stderr.as_raw_fd()] {
                // SAFETY: these are exclusively owned live pipes.
                if unsafe { libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK) } == -1 {
                    let error = std::io::Error::last_os_error();
                    signal(&child, libc::SIGKILL);
                    let _ = child.wait();
                    let _ = ready.send(Err(error));
                    return;
                }
            }
            let delivered = ready.send(Ok((stdin, stdout, stderr))).is_ok();
            let mut deadline = None;
            if !delivered {
                signal(&child, libc::SIGTERM);
                deadline = Some(Instant::now() + TERM_GRACE);
            }
            let result = loop {
                match stopping.recv_timeout(Duration::from_millis(20)) {
                    Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) if deadline.is_none() => {
                        signal(&child, libc::SIGTERM);
                        deadline = Some(Instant::now() + TERM_GRACE);
                    }
                    _ => {}
                }
                if let Some(deadline) = deadline {
                    if Instant::now() < deadline {
                        continue;
                    }
                } else {
                    match exited(&child) {
                        Ok(false) => continue,
                        Ok(true) => {}
                        Err(error) => {
                            signal(&child, libc::SIGKILL);
                            let _ = child.wait();
                            break Err(error);
                        }
                    }
                }
                // Kill remaining group members before reaping the leader: its unreaped
                // PID reserves the group number, so numeric signalling is still safe.
                signal(&child, libc::SIGKILL);
                break child.wait();
            };
            drop(_runtime);
            let _ = finished.send(result);
        })?;
    let owner = Owner { stop, exit };
    let (stdin, stdout, stderr) = started.await.context("Claude spawner stopped")??;
    Ok((
        Spawned {
            owner,
            stdin,
            stdout,
            stderr,
        },
        prompt,
    ))
}

fn signal(child: &Child, signal: libc::c_int) {
    // SAFETY: the direct child is unreaped and owns this process group.
    unsafe {
        libc::kill(-(child.id() as i32), signal);
    }
}

fn exited(child: &Child) -> std::io::Result<bool> {
    // WNOWAIT observes exit without releasing the group leader's PID.
    // SAFETY: info is writable, and waitid only observes our direct child.
    unsafe {
        let mut info: libc::siginfo_t = std::mem::zeroed();
        if libc::waitid(
            libc::P_PID,
            child.id(),
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        ) != 0
        {
            return Err(std::io::Error::last_os_error());
        }
        Ok(info.si_pid() != 0)
    }
}

pub(super) async fn read<T: AsRawFd + Read>(
    pipe: &mut AsyncFd<T>,
    bytes: &mut [u8],
) -> std::io::Result<usize> {
    loop {
        let mut ready = pipe.readable_mut().await?;
        if let Ok(result) = ready.try_io(|pipe| pipe.get_mut().read(bytes)) {
            return result;
        }
    }
}

pub(super) async fn write(stdin: ChildStdin, prompt: &[u8]) -> Result<()> {
    let mut pipe = AsyncFd::new(stdin)?;
    let mut offset = 0;
    while offset < prompt.len() {
        let mut ready = pipe.writable_mut().await?;
        if let Ok(result) = ready.try_io(|pipe| pipe.get_mut().write(&prompt[offset..])) {
            let size = result?;
            anyhow::ensure!(size != 0, "Claude closed prompt input");
            offset += size;
        }
    }
    Ok(())
}
