//! Child processes: combined stdout/stderr through one pipe, deadlines,
//! cancellation, and killing whole process trees.

use std::io::Read;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use tokio::sync::{mpsc, watch};
use tokio::time::Instant;

/// Grace period between asking a process tree to stop and killing it.
const KILL_GRACE: Duration = Duration::from_secs(5);
/// How long to keep reading output after the process exited (background
/// children may hold the pipe open).
const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// Where a process's output goes.
pub trait Output: Send {
    fn data(&mut self, chunk: &[u8]);
    /// Called about once a second while the process runs.
    fn tick(&mut self) {}
    fn finish(&mut self) {}
}

impl Output for Vec<u8> {
    fn data(&mut self, chunk: &[u8]) {
        self.extend_from_slice(chunk);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Exited(i32),
    TimedOut,
    Cancelled,
}

/// Runs a job's processes and remembers them so leftovers (background
/// processes) can be killed when the job ends.
pub struct Exec {
    cancel: watch::Receiver<bool>,
    groups: Vec<u32>,
}

fn exit_code(s: ExitStatus) -> i32 {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        s.code().unwrap_or_else(|| 128 + s.signal().unwrap_or(0))
    }
    #[cfg(not(unix))]
    {
        s.code().unwrap_or(1)
    }
}

#[cfg(unix)]
fn signal_group(pid: u32, sig: libc::c_int) {
    // SAFETY: killpg only sends a signal; a stale group id fails with ESRCH.
    unsafe {
        libc::killpg(pid as libc::pid_t, sig);
    }
}

/// Ask a process tree to stop (`force`: kill it outright).
async fn kill_tree(pid: u32, force: bool) {
    #[cfg(unix)]
    {
        signal_group(pid, if force { libc::SIGKILL } else { libc::SIGTERM });
    }
    #[cfg(windows)]
    {
        let _ = force;
        let _ = tokio::process::Command::new("taskkill")
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
    }
}

impl Exec {
    pub fn new(cancel: watch::Receiver<bool>) -> Self {
        Exec { cancel, groups: Vec::new() }
    }

    pub fn cancelled(&self) -> bool {
        *self.cancel.borrow()
    }

    /// Run `cmd` to completion, streaming its combined output to `out`.
    pub async fn run(
        &mut self,
        mut cmd: std::process::Command,
        out: &mut dyn Output,
        deadline: Option<Instant>,
    ) -> std::io::Result<Outcome> {
        if self.cancelled() {
            return Ok(Outcome::Cancelled);
        }
        let (reader, writer) = std::io::pipe()?;
        cmd.stdin(Stdio::null()).stdout(writer.try_clone()?).stderr(writer);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
            cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);
        }
        let mut cmd = tokio::process::Command::from(cmd);
        let spawned = cmd.spawn();
        // Close our copies of the pipe's write end, or it never reaches EOF.
        drop(cmd);
        let mut child = spawned?;
        let pid = child.id();
        if let Some(p) = pid {
            self.groups.push(p);
        }
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(32);
        std::thread::spawn(move || {
            let mut reader = reader;
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if tx.blocking_send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
        });

        let far = Instant::now() + Duration::from_secs(100 * 365 * 24 * 3600);
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.tick().await;
        let mut status: Option<ExitStatus> = None;
        let mut stopped: Option<Outcome> = None;
        let mut kill_at: Option<Instant> = None;
        let mut drain_until: Option<Instant> = None;
        let mut eof = false;
        let mut cancel = self.cancel.clone();
        let mut cancel_open = true;
        loop {
            if status.is_some() && (eof || drain_until.is_some_and(|d| Instant::now() >= d)) {
                break;
            }
            tokio::select! {
                chunk = rx.recv(), if !eof => match chunk {
                    Some(c) => out.data(&c),
                    None => eof = true,
                },
                st = child.wait(), if status.is_none() => {
                    status = Some(st?);
                    drain_until = Some(Instant::now() + DRAIN_GRACE);
                }
                _ = tokio::time::sleep_until(deadline.unwrap_or(far)), if deadline.is_some() && stopped.is_none() && status.is_none() => {
                    stopped = Some(Outcome::TimedOut);
                }
                ok = async { cancel.wait_for(|c| *c).await.is_ok() }, if cancel_open && stopped.is_none() && status.is_none() => {
                    if ok {
                        stopped = Some(Outcome::Cancelled);
                    } else {
                        cancel_open = false;
                    }
                }
                _ = tokio::time::sleep_until(kill_at.unwrap_or(far)), if kill_at.is_some() && status.is_none() => {
                    if let Some(p) = pid {
                        kill_tree(p, true).await;
                    }
                    kill_at = None;
                }
                _ = tokio::time::sleep_until(drain_until.unwrap_or(far)), if drain_until.is_some() => {}
                _ = tick.tick() => out.tick(),
            }
            if stopped.is_some() && kill_at.is_none() && status.is_none() {
                if let Some(p) = pid {
                    kill_tree(p, false).await;
                }
                kill_at = Some(Instant::now() + KILL_GRACE);
            }
        }
        out.finish();
        let status = status.expect("loop ends after exit");
        Ok(stopped.unwrap_or(Outcome::Exited(exit_code(status))))
    }

    /// Kill every process started so far that is still around.
    pub async fn kill_all(&mut self) {
        for pid in std::mem::take(&mut self.groups) {
            kill_tree(pid, true).await;
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn sh(script: &str) -> std::process::Command {
        let mut c = std::process::Command::new("sh");
        c.args(["-c", script]);
        c
    }

    #[tokio::test]
    async fn combined_output_and_exit_codes() {
        let (_tx, rx) = watch::channel(false);
        let mut exec = Exec::new(rx);
        let mut out = Vec::new();
        let r = exec.run(sh("echo out; echo err >&2; exit 3"), &mut out, None).await.unwrap();
        assert_eq!(r, Outcome::Exited(3));
        assert_eq!(String::from_utf8(out).unwrap(), "out\nerr\n");
    }

    #[tokio::test]
    async fn timeouts_kill_the_process_group() {
        let (_tx, rx) = watch::channel(false);
        let mut exec = Exec::new(rx);
        let mut out = Vec::new();
        let started = std::time::Instant::now();
        let deadline = Some(Instant::now() + Duration::from_millis(300));
        let r = exec.run(sh("sleep 30 & sleep 30; echo never"), &mut out, deadline).await.unwrap();
        assert_eq!(r, Outcome::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());
        assert!(!String::from_utf8(out).unwrap().contains("never"));
    }

    #[tokio::test]
    async fn cancellation() {
        let (tx, rx) = watch::channel(false);
        let mut exec = Exec::new(rx);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            tx.send_replace(true);
        });
        let mut out = Vec::new();
        let r = exec.run(sh("echo started; sleep 30"), &mut out, None).await.unwrap();
        assert_eq!(r, Outcome::Cancelled);
        assert_eq!(String::from_utf8(out).unwrap(), "started\n");
        assert_eq!(exec.run(sh("true"), &mut Vec::new(), None).await.unwrap(), Outcome::Cancelled);
    }

    #[tokio::test]
    async fn background_children_do_not_block() {
        let (_tx, rx) = watch::channel(false);
        let mut exec = Exec::new(rx);
        let started = std::time::Instant::now();
        let r = exec.run(sh("sleep 30 & echo bg"), &mut Vec::new(), None).await.unwrap();
        assert_eq!(r, Outcome::Exited(0));
        assert!(started.elapsed() < Duration::from_secs(10));
        exec.kill_all().await;
    }
}
