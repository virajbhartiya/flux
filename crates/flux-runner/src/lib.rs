//! flux-runner: claims jobs from a flux server's built-in CI and runs them.
//!
//! The loop: long-poll `POST /api/v1/ci/claim` with the runner's labels,
//! run the job in a fresh directory while streaming its log (which also
//! keeps the lease), then report the conclusion. Several jobs may run at
//! once (`--concurrency`). The first SIGINT/SIGTERM stops claiming and lets
//! running jobs finish; a second one cancels them.

pub mod actions;
pub mod client;
pub mod commands;
pub mod exec;
pub mod expr;
pub mod job;
pub mod log;
pub mod mask;
pub mod output;
pub mod shell;
pub mod spec;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use tokio::sync::watch;

use crate::client::Client;
use crate::job::{Finished, RunnerInfo};
use crate::shell::Os;
use crate::spec::ClaimedJob;

/// Environment variables the runner reads its own configuration from;
/// never passed on to jobs.
pub const PRIVATE_ENV: [&str; 2] = ["FLUX_RUNNER_TOKEN", "FLUX_SERVER"];

#[derive(Debug, Clone)]
pub struct Config {
    pub server: String,
    pub token: String,
    pub labels: Vec<String>,
    pub work_dir: PathBuf,
    pub concurrency: usize,
    /// Exit once no job is waiting.
    pub once: bool,
    /// Long-poll duration for claims, in seconds.
    pub wait: u64,
    pub name: String,
}

/// The architecture label: `x64`, `arm64`, `x86`, `arm`, or Rust's name.
pub fn arch_label() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "x86",
        "arm" => "arm",
        other => other,
    }
}

/// `self-hosted`, the OS, the architecture, and the matching
/// `<os>-latest` label.
pub fn default_labels() -> Vec<String> {
    let os = Os::current();
    let latest = match os {
        Os::Linux => "ubuntu-latest",
        Os::MacOs => "macos-latest",
        Os::Windows => "windows-latest",
    };
    ["self-hosted", os.label(), arch_label(), latest].into_iter().map(String::from).collect()
}

fn hostname() -> String {
    ["HOSTNAME", "COMPUTERNAME"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()))
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()))
        .unwrap_or_else(|| "flux-runner".into())
}

/// The runner's name when none is given.
pub fn default_name() -> String {
    hostname()
}

struct Backoff(Duration);

impl Backoff {
    const MIN: Duration = Duration::from_secs(1);
    const MAX: Duration = Duration::from_secs(60);

    fn next(&mut self) -> Duration {
        let d = self.0;
        self.0 = (self.0 * 2).min(Self::MAX);
        // Up to 25% jitter, so a fleet does not retry in lockstep.
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|t| t.subsec_nanos()).unwrap_or(0);
        d + d.mul_f64(f64::from(nanos % 1000) / 4000.0)
    }

    fn reset(&mut self) {
        self.0 = Self::MIN;
    }
}

/// Shutdown level: 0 running, 1 draining (no new jobs), 2 cancel running jobs.
type Shutdown = watch::Receiver<u8>;

async fn signals(tx: watch::Sender<u8>) {
    #[cfg(unix)]
    let mut term = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(s) => Some(s),
        Err(e) => {
            tracing::warn!(error = %e, "cannot listen for SIGTERM");
            None
        }
    };
    loop {
        #[cfg(unix)]
        let got = tokio::select! {
            r = tokio::signal::ctrl_c() => r.is_ok(),
            Some(_) = async { match term.as_mut() { Some(t) => t.recv().await, None => std::future::pending().await } } => true,
        };
        #[cfg(not(unix))]
        let got = tokio::signal::ctrl_c().await.is_ok();
        if !got {
            return;
        }
        let level = (*tx.borrow() + 1).min(2);
        tx.send_replace(level);
        if level == 1 {
            tracing::info!("shutting down: finishing running jobs (signal again to cancel them)");
        } else {
            tracing::warn!("cancelling running jobs");
        }
    }
}

/// Run until shut down (or, with `once`, until no job is waiting).
pub async fn run(cfg: Config) -> anyhow::Result<()> {
    let client = Client::new(&cfg.server, &cfg.token)?;
    std::fs::create_dir_all(&cfg.work_dir).with_context(|| format!("creating work dir {}", cfg.work_dir.display()))?;
    let env =
        std::env::vars_os().filter(|(k, _)| !PRIVATE_ENV.iter().any(|p| k.to_string_lossy().eq_ignore_ascii_case(p))).collect();
    let info = Arc::new(RunnerInfo {
        name: cfg.name.clone(),
        os: Os::current(),
        arch: arch_label().to_string(),
        server: client.base().to_string(),
        env,
    });
    let (tx, shutdown) = watch::channel(0u8);
    let signal_task = tokio::spawn(signals(tx));
    tracing::info!(server = %client.base(), labels = %cfg.labels.join(","), concurrency = cfg.concurrency, "flux-runner started");
    let cfg = Arc::new(cfg);
    let mut workers = tokio::task::JoinSet::new();
    for slot in 0..cfg.concurrency.max(1) {
        workers.spawn(worker(slot, cfg.clone(), client.clone(), info.clone(), shutdown.clone()));
    }
    let mut result = Ok(());
    while let Some(r) = workers.join_next().await {
        match r {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                result = Err(e);
                // A fatal error (bad token) stops every worker.
                workers.abort_all();
            }
            Err(e) if e.is_cancelled() => {}
            Err(e) => result = Err(anyhow::anyhow!("worker panicked: {e}")),
        }
    }
    signal_task.abort();
    result
}

async fn worker(
    slot: usize,
    cfg: Arc<Config>,
    client: Client,
    info: Arc<RunnerInfo>,
    mut shutdown: Shutdown,
) -> anyhow::Result<()> {
    let mut backoff = Backoff(Backoff::MIN);
    loop {
        if *shutdown.borrow() > 0 {
            return Ok(());
        }
        let claimed = tokio::select! {
            r = client.claim(&cfg.labels, cfg.wait) => r,
            _ = shutdown.wait_for(|s| *s > 0) => return Ok(()),
        };
        match claimed {
            Ok(Some(job)) => {
                backoff.reset();
                execute(job, &cfg, &client, &info, &shutdown).await;
            }
            Ok(None) => {
                backoff.reset();
                if cfg.once {
                    return Ok(());
                }
            }
            Err(e) if matches!(e.status, Some(401 | 403 | 404)) => {
                anyhow::bail!("the server rejected the runner: {e} (check --server and the runner token)");
            }
            Err(e) => {
                let delay = backoff.next();
                tracing::warn!(slot, error = %e, retry_in = ?delay, "claiming a job failed");
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {}
                    _ = shutdown.wait_for(|s| *s > 0) => return Ok(()),
                }
            }
        }
    }
}

async fn execute(job: ClaimedJob, cfg: &Config, client: &Client, info: &Arc<RunnerInfo>, shutdown: &Shutdown) {
    let id = job.id;
    tracing::info!(job = id, run = job.run_id, repo = %job.repo, name = %job.spec.name, "running job");
    let (cancel_tx, cancel_rx) = watch::channel(false);
    let cancel_tx = Arc::new(cancel_tx);
    let log = log::JobLog::new();
    let uploader = log::spawn_uploader(client.clone(), id, log.clone(), cancel_tx.clone());
    let forced = {
        let tx = cancel_tx.clone();
        let mut sd = shutdown.clone();
        tokio::spawn(async move {
            if sd.wait_for(|s| *s >= 2).await.is_ok() {
                tx.send_replace(true);
            }
        })
    };
    let root = cfg.work_dir.join(format!("job-{id}-{:x}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let task = tokio::spawn(job::run(job, info.clone(), client.clone(), log.clone(), cancel_rx, root.clone()));
    let finished = match task.await {
        Ok(f) => f,
        Err(e) => {
            log.line(format!("##[error]flux-runner failed while running this job: {e}"));
            Finished { conclusion: "failure", exit_code: Some(1) }
        }
    };
    forced.abort();
    let removed = tokio::task::spawn_blocking({
        let root = root.clone();
        move || remove_dir(&root)
    })
    .await;
    if !matches!(removed, Ok(Ok(()))) {
        tracing::warn!(job = id, dir = %root.display(), "could not remove the job directory");
    }
    log.line(format!("Job finished: {}", finished.conclusion));
    log.close();
    let lost = match tokio::time::timeout(Duration::from_secs(120), uploader).await {
        Ok(Ok(lost)) => lost,
        _ => {
            tracing::warn!(job = id, "could not upload the end of the log");
            false
        }
    };
    if lost {
        tracing::warn!(job = id, "the job was taken away from this runner; not reporting a result");
        return;
    }
    let mut backoff = Backoff(Backoff::MIN);
    let give_up = tokio::time::Instant::now() + Duration::from_secs(600);
    loop {
        match client.finish(id, finished.conclusion, finished.exit_code).await {
            Ok(()) => break,
            Err(e) if e.retryable() && tokio::time::Instant::now() < give_up => {
                let delay = backoff.next();
                tracing::warn!(job = id, error = %e, retry_in = ?delay, "reporting the result failed");
                tokio::time::sleep(delay).await;
            }
            Err(e) => {
                tracing::error!(job = id, error = %e, "could not report the job's result");
                return;
            }
        }
    }
    tracing::info!(job = id, conclusion = finished.conclusion, exit_code = ?finished.exit_code, "job finished");
}

/// Remove a directory tree, making read-only entries writable if needed.
fn remove_dir(root: &std::path::Path) -> std::io::Result<()> {
    if !root.exists() {
        return Ok(());
    }
    if std::fs::remove_dir_all(root).is_ok() {
        return Ok(());
    }
    fn writable(p: &std::path::Path) {
        if let Ok(m) = std::fs::symlink_metadata(p) {
            if m.file_type().is_symlink() {
                return;
            }
            let mut perm = m.permissions();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                perm.set_mode(perm.mode() | 0o700);
            }
            #[cfg(not(unix))]
            #[allow(clippy::permissions_set_readonly_false)]
            perm.set_readonly(false);
            let _ = std::fs::set_permissions(p, perm);
            if m.is_dir()
                && let Ok(entries) = std::fs::read_dir(p)
            {
                for e in entries.flatten() {
                    writable(&e.path());
                }
            }
        }
    }
    writable(root);
    std::fs::remove_dir_all(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels() {
        let l = default_labels();
        assert_eq!(l[0], "self-hosted");
        assert_eq!(l.len(), 4);
        assert!(l.iter().any(|x| x.ends_with("-latest")));
    }

    #[test]
    fn backoff_grows_and_caps() {
        let mut b = Backoff(Backoff::MIN);
        let first = b.next();
        assert!(first >= Duration::from_secs(1) && first < Duration::from_millis(1300));
        for _ in 0..10 {
            b.next();
        }
        assert!(b.next() <= Backoff::MAX.mul_f64(1.25));
        b.reset();
        assert!(b.next() < Duration::from_secs(2));
    }

    #[cfg(unix)]
    #[test]
    fn removes_read_only_trees() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("job");
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        std::fs::write(root.join("a/b/f"), "x").unwrap();
        std::fs::set_permissions(root.join("a/b"), std::fs::Permissions::from_mode(0o500)).unwrap();
        remove_dir(&root).unwrap();
        assert!(!root.exists());
    }
}
