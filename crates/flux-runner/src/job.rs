//! Running one claimed job: contexts and expressions, the job condition,
//! steps (scripts and actions), file commands, the job container, timeouts
//! and the conclusion reported to the server.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Map, Value, json};
use tokio::sync::watch;
use tokio::time::Instant;

use crate::actions::{self, Handler};
use crate::client::Client;
use crate::commands;
use crate::exec::{Exec, Outcome};
use crate::expr::{self, Contexts, JobStatus};
use crate::log::JobLog;
use crate::output::StepOutput;
use crate::shell::{self, Os};
use crate::spec::{ClaimedJob, StepSpec};

/// Where the runner runs and what it may pass on to jobs.
#[derive(Debug, Clone)]
pub struct RunnerInfo {
    pub name: String,
    pub os: Os,
    /// Architecture label (`x64`, `arm64`, …).
    pub arch: String,
    /// The flux server's base URL.
    pub server: String,
    /// The runner's environment minus its own credentials; the base of
    /// every step's environment.
    pub env: Vec<(OsString, OsString)>,
}

impl RunnerInfo {
    fn path(&self) -> OsString {
        self.env
            .iter()
            .find(|(k, _)| k.to_string_lossy().eq_ignore_ascii_case("PATH"))
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    }

    fn runner_arch(&self) -> String {
        match self.arch.as_str() {
            "x64" => "X64".into(),
            "arm64" => "ARM64".into(),
            "x86" => "X86".into(),
            "arm" => "ARM".into(),
            other => other.to_ascii_uppercase(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finished {
    pub conclusion: &'static str,
    pub exit_code: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StepResult {
    Success,
    Failure(i64),
    Skipped,
    Cancelled,
    JobTimedOut,
}

/// Where a step's process sees the job's directories.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Place {
    Host,
    /// Inside the job container (`/__w`, `/__t`).
    Container,
    /// Inside a `docker://` action (`/github/workspace`, …).
    Action,
}

const C_WORKSPACE: &str = "/__w";
const C_TEMP: &str = "/__t";
const A_WORKSPACE: &str = "/github/workspace";
const A_FILES: &str = "/github/file_commands";
const A_TEMP: &str = "/github/runner_temp";
const FILES_DIR: &str = "_runner_file_commands";
const TIMEOUT_EXIT: i64 = 124;

struct Container {
    name: String,
    has_bash: bool,
    path: String,
}

struct StepFiles {
    output: PathBuf,
    env: PathBuf,
    path: PathBuf,
    summary: PathBuf,
}

struct Job {
    job: ClaimedJob,
    info: Arc<RunnerInfo>,
    client: Client,
    log: JobLog,
    exec: Exec,
    workspace: PathBuf,
    temp: PathBuf,
    ctx: Contexts,
    job_env: BTreeMap<String, String>,
    /// Set through `GITHUB_ENV`.
    set_env: BTreeMap<String, String>,
    /// Through `GITHUB_PATH`, most recent first.
    path_add: Vec<String>,
    container: Option<Container>,
    deadline: Instant,
    failed: bool,
    exit_code: Option<i64>,
    cancelled: bool,
    timed_out: bool,
}

/// Run `job` in `root` (a fresh directory the caller removes afterwards).
pub async fn run(
    job: ClaimedJob,
    info: Arc<RunnerInfo>,
    client: Client,
    log: JobLog,
    cancel: watch::Receiver<bool>,
    root: PathBuf,
) -> Finished {
    let deadline = Instant::now() + Duration::from_secs(job.spec.timeout_minutes.max(1) * 60);
    let mut j = Job {
        info,
        client,
        log,
        exec: Exec::new(cancel),
        workspace: root.join("workspace"),
        temp: root.join("temp"),
        ctx: Contexts::new(),
        job_env: BTreeMap::new(),
        set_env: BTreeMap::new(),
        path_add: Vec::new(),
        container: None,
        deadline,
        failed: false,
        exit_code: None,
        cancelled: false,
        timed_out: false,
        job,
    };
    let finished = j.run().await;
    j.cleanup().await;
    finished
}

fn text(v: Option<&Value>) -> String {
    v.map(expr::to_text).unwrap_or_default()
}

fn path_str(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

fn nonce() -> String {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
    format!("{:x}{:x}", std::process::id(), nanos)
}

impl Job {
    async fn run(&mut self) -> Finished {
        for v in self.job.secrets.values() {
            self.log.add_mask(v);
        }
        self.log.add_mask(&self.job.token);
        let spec = self.job.spec.clone();
        self.log.line(format!(
            "flux-runner {} on {} ({}/{})",
            env!("CARGO_PKG_VERSION"),
            self.info.name,
            self.info.os.label(),
            self.info.arch
        ));
        self.log.line(format!("Job: {} / {} (job {}, run {})", spec.workflow, spec.name, self.job.id, self.job.run_id));
        let rev = if self.job.git_sha.is_empty() { &self.job.changeset } else { &self.job.git_sha };
        self.log.line(format!("Repository: {} at {rev}", self.job.repo));
        for dir in [self.workspace.clone(), self.temp.join(FILES_DIR)] {
            if let Err(e) = std::fs::create_dir_all(&dir) {
                return self.fail_setup(format!("cannot create {}: {e}", dir.display()));
            }
        }
        self.init_contexts(Place::Host);

        if let Some(cond) = spec.r#if.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
            self.ctx.status = self.needs_status(cond).await;
            match expr::condition(Some(cond), &self.ctx) {
                Ok(true) => {}
                Ok(false) => {
                    self.log.line(format!("Skipping the job: its condition `{cond}` is false"));
                    return Finished { conclusion: "skipped", exit_code: None };
                }
                Err(e) => return self.fail_setup(format!("invalid job condition `{cond}`: {e}")),
            }
            self.ctx.status = JobStatus::Success;
        }

        for (k, v) in &spec.env {
            match expr::interpolate(v, &self.ctx) {
                Ok(v) => {
                    self.job_env.insert(k.clone(), v);
                    self.refresh_env(&BTreeMap::new());
                }
                Err(e) => return self.fail_setup(format!("env.{k}: {e}")),
            }
        }

        if let Some(image) = spec.container.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
            let image = match expr::interpolate(image, &self.ctx) {
                Ok(i) => i,
                Err(e) => return self.fail_setup(format!("container: {e}")),
            };
            if let Err(e) = self.start_container(&image).await {
                return self.fail_setup(e);
            }
            self.init_contexts(Place::Container);
            self.refresh_env(&BTreeMap::new());
        }

        for (i, step) in spec.steps.iter().enumerate() {
            if self.exec.cancelled() {
                self.cancelled = true;
            }
            if self.cancelled || self.timed_out {
                break;
            }
            if Instant::now() >= self.deadline {
                self.job_timed_out();
                break;
            }
            self.step(i, step).await;
        }
        if self.exec.cancelled() && !self.failed {
            self.cancelled = true;
        }
        self.conclusion()
    }

    fn conclusion(&self) -> Finished {
        if self.cancelled {
            self.log.line("##[error]The job was cancelled.");
            return Finished { conclusion: "cancelled", exit_code: None };
        }
        if self.failed {
            let code = self.exit_code.unwrap_or(1);
            if self.job.spec.continue_on_error {
                self.log.line("##[warning]The job failed, but it has continue-on-error set.");
                return Finished { conclusion: "success", exit_code: Some(code) };
            }
            return Finished { conclusion: "failure", exit_code: Some(code) };
        }
        Finished { conclusion: "success", exit_code: Some(0) }
    }

    fn fail_setup(&mut self, msg: String) -> Finished {
        self.log.line(format!("##[error]{msg}"));
        self.failed = true;
        self.exit_code.get_or_insert(1);
        self.conclusion()
    }

    fn job_timed_out(&mut self) {
        self.log
            .line(format!("##[error]The job exceeded its maximum execution time of {} minutes.", self.job.spec.timeout_minutes));
        self.timed_out = true;
        self.failed = true;
        self.exit_code.get_or_insert(TIMEOUT_EXIT);
    }

    /// Paths as a process in `place` sees them.
    fn map_path(&self, place: Place, p: &Path) -> String {
        let under = |root: &Path, to: &str| {
            p.strip_prefix(root).ok().map(|rest| {
                let rest = rest.to_string_lossy().replace('\\', "/");
                if rest.is_empty() { to.to_string() } else { format!("{to}/{rest}") }
            })
        };
        let mapped = match place {
            Place::Host => None,
            Place::Container => under(&self.workspace, C_WORKSPACE).or_else(|| under(&self.temp, C_TEMP)),
            Place::Action => under(&self.workspace, A_WORKSPACE)
                .or_else(|| under(&self.temp.join(FILES_DIR), A_FILES))
                .or_else(|| under(&self.temp, A_TEMP)),
        };
        mapped.unwrap_or_else(|| path_str(p))
    }

    fn init_contexts(&mut self, place: Place) {
        let spec = &self.job.spec;
        let workspace = self.map_path(place, &self.workspace);
        let temp = self.map_path(place, &self.temp);
        let mut github: Map<String, Value> = spec.context.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        github.insert("workspace".into(), json!(workspace));
        github.insert("run_id".into(), json!(self.job.run_id.to_string()));
        github.insert("job".into(), json!(spec.job));
        github.insert("token".into(), json!(self.job.token));
        github.insert("action".into(), json!(""));
        if !self.job.git_sha.is_empty() {
            github.entry("sha").or_insert_with(|| json!(self.job.git_sha));
        }
        github.entry("repository").or_insert_with(|| json!(self.job.repo));
        github.entry("workflow").or_insert_with(|| json!(spec.workflow));
        github.entry("server_url").or_insert_with(|| json!(self.info.server));
        let owner = self.job.repo.split_once('/').map(|(o, _)| o.to_string()).unwrap_or_default();
        github.entry("repository_owner").or_insert_with(|| json!(owner));
        github.entry("event").or_insert_with(|| json!({}));
        let mut secrets: Map<String, Value> = self.job.secrets.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
        secrets.insert("GITHUB_TOKEN".into(), json!(self.job.token));
        secrets.insert("FLUX_TOKEN".into(), json!(self.job.token));
        let matrix: Map<String, Value> = spec.matrix.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        self.ctx.set("github", Value::Object(github));
        self.ctx.set("matrix", Value::Object(matrix));
        self.ctx.set("secrets", Value::Object(secrets));
        self.ctx.set(
            "runner",
            json!({
                "os": self.info.os.runner_os(),
                "arch": self.info.runner_arch(),
                "name": self.info.name,
                "temp": temp,
                "workspace": workspace,
                "tool_cache": "",
            }),
        );
        let container = self.container.as_ref().map(|c| json!({ "id": c.name })).unwrap_or(Value::Null);
        self.ctx.set("job", json!({ "status": "success", "container": container }));
        for name in ["steps", "env", "inputs", "needs", "vars", "strategy"] {
            self.ctx.object(name);
        }
    }

    fn sync_status(&mut self) {
        self.ctx.status = if self.cancelled {
            JobStatus::Cancelled
        } else if self.failed {
            JobStatus::Failure
        } else {
            JobStatus::Success
        };
        let status = self.ctx.status.as_str();
        self.ctx.object("job").insert("status".into(), json!(status));
    }

    fn refresh_env(&mut self, step_env: &BTreeMap<String, String>) {
        let env: Map<String, Value> =
            self.job_env.iter().chain(self.set_env.iter()).chain(step_env.iter()).map(|(k, v)| (k.clone(), json!(v))).collect();
        self.ctx.set("env", Value::Object(env));
    }

    /// For job conditions using `failure()` / `cancelled()`: the state of
    /// the jobs this one needs, from the server.
    async fn needs_status(&self, cond: &str) -> JobStatus {
        let lower = cond.to_ascii_lowercase();
        if !lower.contains("failure()") && !lower.contains("cancelled()") {
            return JobStatus::Success;
        }
        let jobs = match self.client.run_jobs(&self.job.repo, self.job.run_id, &self.job.token).await {
            Ok(j) => j,
            Err(e) => {
                tracing::warn!(job = self.job.id, error = %e, "cannot read the run's jobs; assuming needed jobs succeeded");
                return JobStatus::Success;
            }
        };
        let Some(me) = jobs.iter().find(|j| j["id"].as_i64() == Some(self.job.id)) else { return JobStatus::Success };
        let needs: Vec<&str> = me["needs"].as_array().map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
        let mut status = JobStatus::Success;
        for j in &jobs {
            let name = j["name"].as_str().unwrap_or_default();
            if j["id"] == me["id"] || !needs.iter().any(|n| name == *n || name.starts_with(&format!("{n} ("))) {
                continue;
            }
            match j["state"].as_str() {
                Some("cancelled") => status = JobStatus::Cancelled,
                Some("failure") if status == JobStatus::Success => status = JobStatus::Failure,
                _ => {}
            }
        }
        status
    }

    fn display_name(&self, idx: usize, step: &StepSpec) -> String {
        if let Some(n) = &step.name {
            return expr::interpolate(n, &self.ctx).unwrap_or_else(|_| n.clone());
        }
        if let Some(r) = &step.run {
            let first = r.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or_default();
            let first = expr::interpolate(first, &self.ctx).unwrap_or_else(|_| first.to_string());
            return if first.chars().count() > 80 {
                format!("{}…", first.chars().take(80).collect::<String>())
            } else {
                first.to_string()
            };
        }
        step.uses.clone().unwrap_or_else(|| format!("step {}", idx + 1))
    }

    async fn step(&mut self, idx: usize, step: &StepSpec) {
        self.sync_status();
        self.refresh_env(&BTreeMap::new());
        let display = self.display_name(idx, step);
        match expr::condition(step.r#if.as_deref(), &self.ctx) {
            Ok(true) => {}
            Ok(false) => {
                self.log.line(format!("Skipping step: {display}"));
                self.record(step, StepResult::Skipped, Vec::new());
                return;
            }
            Err(e) => {
                self.log
                    .line(format!("##[error]{display}: invalid condition `{}`: {e}", step.r#if.as_deref().unwrap_or_default()));
                self.record(step, StepResult::Failure(1), Vec::new());
                return;
            }
        }
        let mut step_env = BTreeMap::new();
        for (k, v) in &step.env {
            match expr::interpolate(v, &self.ctx) {
                Ok(v) => {
                    step_env.insert(k.clone(), v);
                    self.refresh_env(&step_env);
                }
                Err(e) => {
                    self.log.line(format!("##[error]{display}: env.{k}: {e}"));
                    self.record(step, StepResult::Failure(1), Vec::new());
                    return;
                }
            }
        }
        let step_deadline = step.timeout_minutes.map(|m| Instant::now() + Duration::from_secs(m.max(1) * 60));
        let deadline = step_deadline.map_or(self.deadline, |d| d.min(self.deadline));
        let (result, outputs) = if let Some(run) = &step.run {
            self.run_script(idx, step, run, &display, &step_env, deadline).await
        } else if let Some(uses) = &step.uses {
            self.run_action(idx, step, uses, &display, &step_env, deadline).await
        } else {
            self.log.line(format!("##[error]{display}: a step needs `run` or `uses`"));
            (StepResult::Failure(1), Vec::new())
        };
        self.record(step, result, outputs);
    }

    fn record(&mut self, step: &StepSpec, result: StepResult, outputs: Vec<(String, String)>) {
        let (outcome, conclusion) = match result {
            StepResult::Success => ("success", "success"),
            StepResult::Skipped => ("skipped", "skipped"),
            StepResult::Cancelled => {
                self.cancelled = true;
                ("cancelled", "cancelled")
            }
            StepResult::JobTimedOut => {
                self.job_timed_out();
                ("failure", "failure")
            }
            StepResult::Failure(_) if step.continue_on_error => {
                self.log.line("##[warning]The step failed, but it has continue-on-error set; continuing.");
                ("failure", "success")
            }
            StepResult::Failure(code) => {
                self.failed = true;
                self.exit_code.get_or_insert(code);
                ("failure", "failure")
            }
        };
        if let Some(id) = &step.id {
            let outputs: Map<String, Value> = outputs.into_iter().map(|(k, v)| (k, json!(v))).collect();
            self.ctx
                .object("steps")
                .insert(id.clone(), json!({ "outputs": outputs, "outcome": outcome, "conclusion": conclusion }));
        }
    }

    fn outcome_result(&self, o: Outcome, deadline: Instant, step: &StepSpec) -> StepResult {
        match o {
            Outcome::Exited(0) => StepResult::Success,
            Outcome::Exited(code) => {
                self.log.line(format!("##[error]Process completed with exit code {code}."));
                StepResult::Failure(i64::from(code))
            }
            Outcome::Cancelled => {
                self.log.line("##[error]The operation was cancelled.");
                StepResult::Cancelled
            }
            Outcome::TimedOut if deadline >= self.deadline => StepResult::JobTimedOut,
            Outcome::TimedOut => {
                self.log.line(format!(
                    "##[error]The step exceeded its timeout of {} minutes.",
                    step.timeout_minutes.unwrap_or_default()
                ));
                StepResult::Failure(TIMEOUT_EXIT)
            }
        }
    }

    fn prepare_files(&self, idx: usize) -> std::io::Result<StepFiles> {
        let dir = self.temp.join(FILES_DIR);
        let f = StepFiles {
            output: dir.join(format!("output_{idx}")),
            env: dir.join(format!("env_{idx}")),
            path: dir.join(format!("path_{idx}")),
            summary: dir.join(format!("step_summary_{idx}")),
        };
        for p in [&f.output, &f.env, &f.path, &f.summary] {
            std::fs::write(p, b"")?;
        }
        Ok(f)
    }

    /// Apply a finished step's file commands; returns its outputs.
    fn collect_files(&mut self, f: &StepFiles) -> Vec<(String, String)> {
        let read = |p: &Path| std::fs::read(p).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
        let outputs = commands::parse_kv(&read(&f.output)).unwrap_or_else(|e| {
            self.log.line(format!("##[error]GITHUB_OUTPUT: {e}"));
            Vec::new()
        });
        match commands::parse_kv(&read(&f.env)) {
            Ok(vars) => self.set_env.extend(vars),
            Err(e) => self.log.line(format!("##[error]GITHUB_ENV: {e}")),
        }
        for dir in commands::parse_path(&read(&f.path)) {
            self.path_add.retain(|d| *d != dir);
            self.path_add.insert(0, dir);
        }
        let summary = read(&f.summary);
        if !summary.trim().is_empty() {
            self.log.line("##[group]Step summary");
            self.log.write(summary.as_bytes());
            if !summary.ends_with('\n') {
                self.log.line("");
            }
            self.log.line("##[endgroup]");
        }
        outputs
    }

    /// `GITHUB_*`, `RUNNER_*` and `FLUX_*` variables.
    fn default_env(&self, place: Place) -> BTreeMap<String, String> {
        let c = &self.job.spec.context;
        let reference = text(c.get("ref"));
        let ref_type = if reference.starts_with("refs/tags/") { "tag" } else { "branch" };
        let mut e: BTreeMap<String, String> = [
            ("CI", "true".to_string()),
            ("FLUX_CI", "true".to_string()),
            ("GITHUB_WORKSPACE", self.map_path(place, &self.workspace)),
            ("GITHUB_SHA", self.job.git_sha.clone()),
            ("GITHUB_REF", reference),
            ("GITHUB_REF_NAME", text(c.get("ref_name"))),
            ("GITHUB_REF_TYPE", ref_type.to_string()),
            ("GITHUB_REPOSITORY", self.job.repo.clone()),
            ("GITHUB_EVENT_NAME", text(c.get("event_name"))),
            ("GITHUB_ACTOR", text(c.get("actor"))),
            ("GITHUB_SERVER_URL", text(c.get("server_url"))),
            ("GITHUB_RUN_ID", self.job.run_id.to_string()),
            ("GITHUB_JOB", self.job.spec.job.clone()),
            ("GITHUB_WORKFLOW", self.job.spec.workflow.clone()),
            ("RUNNER_OS", self.info.os.runner_os().to_string()),
            ("RUNNER_ARCH", self.info.runner_arch()),
            ("RUNNER_NAME", self.info.name.clone()),
            ("RUNNER_TEMP", self.map_path(place, &self.temp)),
            ("FLUX_SERVER_URL", self.info.server.clone()),
            ("FLUX_REPOSITORY", self.job.repo.clone()),
            ("FLUX_CHANGESET", self.job.changeset.clone()),
            ("FLUX_JOB_ID", self.job.id.to_string()),
            ("FLUX_RUN_ID", self.job.run_id.to_string()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        if e["GITHUB_SERVER_URL"].is_empty() {
            e.insert("GITHUB_SERVER_URL".into(), self.info.server.clone());
        }
        e
    }

    /// A step's full environment (on top of the runner's own).
    fn step_env(&self, place: Place, step_env: &BTreeMap<String, String>, files: &StepFiles) -> BTreeMap<String, String> {
        let mut env = self.default_env(place);
        env.extend(self.job_env.clone());
        env.extend(self.set_env.clone());
        env.extend(step_env.clone());
        env.insert("GITHUB_OUTPUT".into(), self.map_path(place, &files.output));
        env.insert("GITHUB_ENV".into(), self.map_path(place, &files.env));
        env.insert("GITHUB_PATH".into(), self.map_path(place, &files.path));
        env.insert("GITHUB_STEP_SUMMARY".into(), self.map_path(place, &files.summary));
        let user_path = env.get("PATH").cloned();
        match place {
            Place::Host => {
                let base = user_path.map(OsString::from).unwrap_or_else(|| self.info.path());
                let dirs = self.path_add.iter().map(PathBuf::from).chain(std::env::split_paths(&base));
                if let Ok(p) = std::env::join_paths(dirs) {
                    env.insert("PATH".into(), p.to_string_lossy().into_owned());
                }
            }
            Place::Container => {
                let base = user_path.or_else(|| self.container.as_ref().map(|c| c.path.clone())).unwrap_or_default();
                let p = self.path_add.iter().cloned().chain(std::iter::once(base)).filter(|s| !s.is_empty()).collect::<Vec<_>>();
                env.insert("PATH".into(), p.join(":"));
            }
            Place::Action => {
                if user_path.is_none() {
                    env.remove("PATH");
                }
            }
        }
        env
    }

    fn host_command(&self, program: &str) -> std::process::Command {
        let mut c = std::process::Command::new(program);
        c.env_clear().envs(self.info.env.iter().map(|(k, v)| (k, v)));
        c
    }

    /// `docker` with `env` passed by name (values never appear in argv,
    /// except `PATH`).
    fn docker_command(&self, env: &BTreeMap<String, String>) -> (std::process::Command, Vec<String>) {
        let mut c = self.host_command("docker");
        let mut args = Vec::new();
        for (k, v) in env {
            args.push("-e".to_string());
            if k == "PATH" {
                args.push(format!("PATH={v}"));
            } else {
                args.push(k.clone());
                c.env(k, v);
            }
        }
        (c, args)
    }

    fn has_docker(&self) -> bool {
        shell::find_program("docker", &self.info.path()).is_some()
    }

    fn working_dir(&self, step: &StepSpec) -> Result<(PathBuf, String), String> {
        let wd = step.working_directory.as_ref().or(self.job.spec.defaults.working_directory.as_ref());
        let Some(wd) = wd.map(|w| expr::interpolate(w, &self.ctx)).transpose()? else {
            return Ok((self.workspace.clone(), self.map_path(Place::Container, &self.workspace)));
        };
        let p = Path::new(&wd);
        let (host, inside) = if p.is_absolute() || wd.starts_with('/') {
            (p.to_path_buf(), wd.clone())
        } else {
            (self.workspace.join(&wd), format!("{C_WORKSPACE}/{}", wd.trim_start_matches("./")))
        };
        if (self.container.is_none() || host.starts_with(&self.workspace)) && !host.is_dir() {
            return Err(format!("working directory {wd} does not exist"));
        }
        Ok((host, inside))
    }

    async fn run_script(
        &mut self,
        idx: usize,
        step: &StepSpec,
        run: &str,
        display: &str,
        step_env: &BTreeMap<String, String>,
        deadline: Instant,
    ) -> (StepResult, Vec<(String, String)>) {
        self.log.line(format!("##[group]Run {display}"));
        let fail = |this: &Self, msg: String| {
            this.log.line("##[endgroup]");
            this.log.line(format!("##[error]{msg}"));
            (StepResult::Failure(1), Vec::new())
        };
        let script = match expr::interpolate(run, &self.ctx) {
            Ok(s) => s,
            Err(e) => return fail(self, e),
        };
        for l in script.lines() {
            self.log.line(l);
        }
        let place = if self.container.is_some() { Place::Container } else { Place::Host };
        let shell_name = step.shell.as_deref().or(self.job.spec.defaults.shell.as_deref());
        let os = if place == Place::Container { Os::Linux } else { self.info.os };
        let files = match self.prepare_files(idx) {
            Ok(f) => f,
            Err(e) => return fail(self, format!("cannot create step files: {e}")),
        };
        let env = self.step_env(place, step_env, &files);
        let path = OsString::from(env.get("PATH").cloned().unwrap_or_default());
        let container_bash = self.container.as_ref().map(|c| c.has_bash);
        let has = |p: &str| match container_bash {
            Some(bash) => p != "bash" || bash,
            None => shell::find_program(p, &path).is_some(),
        };
        let ext = match shell::shell_command(shell_name, os, &has, "") {
            Ok(s) => s.extension(),
            Err(e) => return fail(self, e),
        };
        let script_path = self.temp.join(format!("step_{idx}{ext}"));
        let sc = match shell::shell_command(shell_name, os, &has, &self.map_path(place, &script_path)) {
            Ok(s) => s,
            Err(e) => return fail(self, e),
        };
        if let Err(e) = std::fs::write(&script_path, sc.wrap(&script)) {
            return fail(self, format!("cannot write the script: {e}"));
        }
        self.log.line(format!("shell: {} {}", sc.program, sc.args.join(" ")));
        let shown: BTreeMap<&String, &String> = self.job_env.iter().chain(self.set_env.iter()).chain(step_env.iter()).collect();
        if !shown.is_empty() {
            self.log.line("env:");
            for (k, v) in shown {
                self.log.line(format!("  {k}: {v}"));
            }
        }
        self.log.line("##[endgroup]");
        let (host_wd, inside_wd) = match self.working_dir(step) {
            Ok(w) => w,
            Err(e) => {
                self.log.line(format!("##[error]{e}"));
                return (StepResult::Failure(1), Vec::new());
            }
        };
        let cmd = match &self.container {
            Some(c) => {
                let (mut cmd, env_args) = self.docker_command(&env);
                cmd.arg("exec").args(["-w", &inside_wd]).args(env_args).arg(&c.name).arg(&sc.program).args(&sc.args);
                cmd
            }
            None => {
                let mut cmd = self.host_command(&sc.program);
                add_shell_args(&mut cmd, &sc);
                cmd.current_dir(&host_wd).envs(&env);
                cmd
            }
        };
        let mut out = StepOutput::new(self.log.clone());
        let result = match self.exec.run(cmd, &mut out, Some(deadline)).await {
            Ok(o) => self.outcome_result(o, deadline, step),
            Err(e) => {
                self.log.line(format!("##[error]Cannot start `{}`: {e}", sc.program));
                StepResult::Failure(if e.kind() == std::io::ErrorKind::NotFound { 127 } else { 1 })
            }
        };
        let _ = std::fs::remove_file(&script_path);
        let mut outputs = std::mem::take(&mut out.outputs);
        outputs.extend(self.collect_files(&files));
        (result, outputs)
    }

    fn eval_with(&self, step: &StepSpec) -> Result<BTreeMap<String, String>, String> {
        step.with
            .iter()
            .map(|(k, v)| Ok((k.clone(), expr::interpolate(v, &self.ctx).map_err(|e| format!("with.{k}: {e}"))?)))
            .collect()
    }

    async fn run_action(
        &mut self,
        idx: usize,
        step: &StepSpec,
        uses: &str,
        display: &str,
        step_env: &BTreeMap<String, String>,
        deadline: Instant,
    ) -> (StepResult, Vec<(String, String)>) {
        self.log.line(format!("##[group]Run {display}"));
        let handler = actions::handler(uses);
        let with = match &handler {
            Handler::Skip(_) | Handler::Unsupported => BTreeMap::new(),
            _ => match self.eval_with(step) {
                Ok(w) => w,
                Err(e) => {
                    self.log.line("##[endgroup]");
                    self.log.line(format!("##[error]{e}"));
                    return (StepResult::Failure(1), Vec::new());
                }
            },
        };
        if !with.is_empty() {
            self.log.line("with:");
            for (k, v) in &with {
                self.log.line(format!("  {k}: {v}"));
            }
        }
        self.log.line("##[endgroup]");
        match handler {
            Handler::Checkout => (self.checkout(&with, deadline, step).await, Vec::new()),
            Handler::Docker(image) => self.docker_action(idx, step, &image, &with, step_env, deadline).await,
            Handler::Skip(note) => {
                self.log.line(format!("##[warning]{uses} skipped: {note}."));
                (StepResult::Skipped, Vec::new())
            }
            Handler::Unsupported => {
                self.log.line(format!("##[warning]action {uses} is not supported by flux-runner; skipping"));
                (StepResult::Skipped, Vec::new())
            }
        }
    }

    /// Run git in `dir`, capturing its output.
    async fn git(
        &mut self,
        dir: &Path,
        auth: &[(String, String)],
        args: &[&str],
        deadline: Instant,
    ) -> std::io::Result<(Outcome, String)> {
        let mut cmd = self.host_command("git");
        cmd.current_dir(dir).args(args).env("GIT_TERMINAL_PROMPT", "0").envs(auth.iter().map(|(k, v)| (k, v)));
        let mut out = Vec::new();
        let o = self.exec.run(cmd, &mut out, Some(deadline)).await?;
        Ok((o, String::from_utf8_lossy(&out).into_owned()))
    }

    /// Native `actions/checkout`: fetch the job's commit (or `with.ref`) into
    /// `with.path` and check it out detached.
    async fn checkout(&mut self, with: &BTreeMap<String, String>, deadline: Instant, step: &StepSpec) -> StepResult {
        let get = |k: &str| with.get(k).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        let rel = get("path").unwrap_or_default();
        if Path::new(&rel).is_absolute() || Path::new(&rel).components().any(|c| matches!(c, Component::ParentDir)) {
            self.log.line(format!("##[error]checkout path {rel:?} must be inside the workspace"));
            return StepResult::Failure(1);
        }
        let depth: u64 = match get("fetch-depth").map(|d| d.parse()) {
            None => 1,
            Some(Ok(d)) => d,
            Some(Err(_)) => {
                self.log.line("##[error]fetch-depth must be a number");
                return StepResult::Failure(1);
            }
        };
        let token = get("token").unwrap_or_else(|| self.job.token.clone());
        self.log.add_mask(&token);
        let url = match get("repository") {
            Some(r) if !r.trim_end_matches(".git").eq_ignore_ascii_case(&self.job.repo) => {
                format!("{}/{}.git", self.info.server.trim_end_matches('/'), r.trim_end_matches(".git"))
            }
            _ => self.job.clone_url.clone(),
        };
        if get("submodules").is_some_and(|s| s != "false") {
            self.log.line("##[warning]submodules are not supported by flux-runner's checkout; they are not fetched");
        }
        if get("lfs").as_deref() == Some("true") {
            self.log.line("##[warning]Git LFS is not supported by flux-runner's checkout");
        }
        let dest = if rel.is_empty() { self.workspace.clone() } else { self.workspace.join(&rel) };
        if let Err(e) = std::fs::create_dir_all(&dest) {
            self.log.line(format!("##[error]cannot create {}: {e}", dest.display()));
            return StepResult::Failure(1);
        }
        let auth = vec![
            ("GIT_CONFIG_COUNT".to_string(), "1".to_string()),
            ("GIT_CONFIG_KEY_0".to_string(), "http.extraHeader".to_string()),
            ("GIT_CONFIG_VALUE_0".to_string(), format!("Authorization: Bearer {token}")),
        ];
        self.log.line(format!("Fetching {url} into {}", self.map_path(Place::Host, &dest)));

        let setup: Vec<Vec<&str>> = if dest.join(".git").exists() {
            let mut s = vec![vec!["remote", "set-url", "origin", url.as_str()]];
            if get("clean").as_deref() != Some("false") {
                s.push(vec!["clean", "-ffdx", "-q"]);
                s.push(vec!["reset", "--hard", "-q", "HEAD"]);
            }
            s
        } else {
            vec![vec!["init", "-q"], vec!["remote", "add", "origin", url.as_str()]]
        };
        for args in setup {
            let required = matches!(args[0], "init" | "remote");
            match self.git(&dest, &[], &args, deadline).await {
                Ok((Outcome::Exited(code), out)) if code != 0 && required => {
                    self.log.write(out.as_bytes());
                    self.log.line(format!("##[error]git {} failed", args[0]));
                    return StepResult::Failure(i64::from(code));
                }
                Ok((Outcome::Exited(_), _)) => {}
                Ok((o, _)) => return self.outcome_result(o, deadline, step),
                Err(e) => {
                    self.log.line(format!("##[error]git is required for actions/checkout: {e}"));
                    return StepResult::Failure(1);
                }
            }
        }

        let all = ["+refs/heads/*:refs/remotes/origin/*", "+refs/tags/*:refs/tags/*"];
        let depth_arg = format!("--depth={depth}");
        let fetch = |depth: bool, refspecs: &[&str]| -> Vec<String> {
            let mut a: Vec<String> = ["fetch", "--no-tags", "--prune", "--no-recurse-submodules"].map(String::from).to_vec();
            if depth {
                a.push(depth_arg.clone());
            }
            a.push("origin".into());
            a.extend(refspecs.iter().map(|s| s.to_string()));
            a
        };
        // (fetch arguments, what to check out afterwards)
        let mut attempts: Vec<(Vec<String>, String)> = Vec::new();
        let wanted = get("ref").unwrap_or_else(|| {
            if self.job.git_sha.is_empty() { text(self.job.spec.context.get("ref")) } else { self.job.git_sha.clone() }
        });
        if actions::is_sha(&wanted) {
            if depth == 0 {
                attempts.push((fetch(false, &all), wanted.clone()));
            } else {
                attempts.push((fetch(true, &[wanted.as_str()]), wanted.clone()));
            }
            attempts.push((fetch(false, &[wanted.as_str()]), wanted.clone()));
            attempts.push((fetch(false, &all), wanted.clone()));
        } else {
            for (remote, local) in actions::ref_candidates(&wanted) {
                let spec = format!("+{remote}:{local}");
                if depth > 0 {
                    attempts.push((fetch(true, &[spec.as_str()]), local.clone()));
                }
                attempts.push((fetch(false, &[spec.as_str()]), local));
            }
        }
        let mut target = None;
        let mut last = String::new();
        for (i, (args, checkout)) in attempts.iter().enumerate() {
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            match self.git(&dest, &auth, &args, deadline).await {
                Ok((Outcome::Exited(0), out)) => {
                    if i > 0 && depth > 0 && last.contains("shallow") {
                        self.log.line("The server does not support shallow fetches; fetched the full history instead.");
                    }
                    self.log.write(out.as_bytes());
                    target = Some(checkout.clone());
                    break;
                }
                Ok((Outcome::Exited(_), out)) => last = out,
                Ok((o, _)) => return self.outcome_result(o, deadline, step),
                Err(e) => {
                    self.log.line(format!("##[error]cannot run git: {e}"));
                    return StepResult::Failure(1);
                }
            }
        }
        let Some(target) = target else {
            self.log.write(last.as_bytes());
            self.log.line(format!("##[error]could not fetch {wanted} from {url}"));
            return StepResult::Failure(1);
        };
        let persist = get("persist-credentials").as_deref() != Some("false");
        let header = format!("Authorization: Bearer {token}");
        let mut finish: Vec<Vec<&str>> = vec![vec!["checkout", "--force", "--quiet", "--detach", target.as_str()]];
        if persist {
            finish.push(vec!["config", "--local", "http.extraHeader", header.as_str()]);
        }
        finish.push(vec!["log", "-1", "--format=HEAD is now at %h %s"]);
        for args in finish {
            match self.git(&dest, &[], &args, deadline).await {
                Ok((Outcome::Exited(0), out)) => self.log.write(out.as_bytes()),
                Ok((Outcome::Exited(code), out)) => {
                    self.log.write(out.as_bytes());
                    self.log.line(format!("##[error]git {} failed", args[0]));
                    return StepResult::Failure(i64::from(code));
                }
                Ok((o, _)) => return self.outcome_result(o, deadline, step),
                Err(e) => {
                    self.log.line(format!("##[error]cannot run git: {e}"));
                    return StepResult::Failure(1);
                }
            }
        }
        StepResult::Success
    }

    /// `uses: docker://image`: run the image with the workspace mounted at
    /// `/github/workspace`, inputs as `INPUT_*`, `with.args` / `with.entrypoint`.
    async fn docker_action(
        &mut self,
        idx: usize,
        step: &StepSpec,
        image: &str,
        with: &BTreeMap<String, String>,
        step_env: &BTreeMap<String, String>,
        deadline: Instant,
    ) -> (StepResult, Vec<(String, String)>) {
        if !self.has_docker() {
            self.log.line(format!("##[error]docker is required to run docker://{image}, but it is not installed on this runner"));
            return (StepResult::Failure(1), Vec::new());
        }
        let files = match self.prepare_files(idx) {
            Ok(f) => f,
            Err(e) => {
                self.log.line(format!("##[error]cannot create step files: {e}"));
                return (StepResult::Failure(1), Vec::new());
            }
        };
        let mut env = self.step_env(Place::Action, step_env, &files);
        for (k, v) in with.iter().filter(|(k, _)| !matches!(k.as_str(), "args" | "entrypoint")) {
            env.insert(format!("INPUT_{}", k.replace(' ', "_").to_ascii_uppercase()), v.clone());
        }
        let args = match with.get("args").map(|a| shell::split_words(a)).transpose() {
            Ok(a) => a.unwrap_or_default(),
            Err(e) => {
                self.log.line(format!("##[error]with.args: {e}"));
                return (StepResult::Failure(1), Vec::new());
            }
        };
        let (mut cmd, env_args) = self.docker_command(&env);
        let mount = |host: &Path, inside: &str| format!("{}:{inside}", path_str(host));
        cmd.args(["run", "--rm", "--name", &format!("flux-{}-{idx}-{}", self.job.id, nonce())])
            .args(["--label", &format!("flux-runner.job={}", self.job.id)])
            .args(["-w", A_WORKSPACE])
            .args(["-v", &mount(&self.workspace, A_WORKSPACE)])
            .args(["-v", &mount(&self.temp.join(FILES_DIR), A_FILES)])
            .args(["-v", &mount(&self.temp, A_TEMP)])
            .args(env_args);
        if let Some(ep) = with.get("entrypoint").filter(|e| !e.trim().is_empty()) {
            cmd.args(["--entrypoint", ep.trim()]);
        }
        cmd.arg(image).args(args);
        let mut out = StepOutput::new(self.log.clone());
        let result = match self.exec.run(cmd, &mut out, Some(deadline)).await {
            Ok(o) => self.outcome_result(o, deadline, step),
            Err(e) => {
                self.log.line(format!("##[error]cannot run docker: {e}"));
                StepResult::Failure(1)
            }
        };
        let mut outputs = std::mem::take(&mut out.outputs);
        outputs.extend(self.collect_files(&files));
        (result, outputs)
    }

    /// Start the job container (`container:`); steps then run in it with
    /// `docker exec`.
    async fn start_container(&mut self, image: &str) -> Result<(), String> {
        if !self.has_docker() {
            return Err(format!("this job runs in the container {image}, but docker is not available on this runner"));
        }
        let name = format!("flux-job-{}-{}", self.job.id, nonce());
        self.log.line(format!("##[group]Starting job container {image}"));
        let mut cmd = self.host_command("docker");
        cmd.args(["run", "-d", "--name", &name])
            .args(["--label", &format!("flux-runner.job={}", self.job.id)])
            .args(["-v", &format!("{}:{C_WORKSPACE}", path_str(&self.workspace))])
            .args(["-v", &format!("{}:{C_TEMP}", path_str(&self.temp))])
            .args(["-w", C_WORKSPACE, "--entrypoint", "tail", image, "-f", "/dev/null"]);
        let mut out = StepOutput::new(self.log.clone());
        let started = self.exec.run(cmd, &mut out, Some(self.deadline)).await;
        // Registered before checking, so cleanup removes a half-started one.
        self.container = Some(Container { name: name.clone(), has_bash: false, path: String::new() });
        match started {
            Ok(Outcome::Exited(0)) => {}
            Ok(o) => {
                self.log.line("##[endgroup]");
                return Err(format!("could not start the job container {image} ({o:?})"));
            }
            Err(e) => {
                self.log.line("##[endgroup]");
                return Err(format!("could not run docker: {e}"));
            }
        }
        let mut probe = self.host_command("docker");
        probe.args([
            "exec",
            &name,
            "sh",
            "-c",
            "if command -v bash >/dev/null 2>&1; then echo yes; else echo no; fi; printf '%s\\n' \"$PATH\"",
        ]);
        let mut out = Vec::new();
        let probed = self.exec.run(probe, &mut out, Some(self.deadline)).await;
        let text = String::from_utf8_lossy(&out);
        let mut lines = text.lines();
        let (has_bash, path) = match probed {
            Ok(Outcome::Exited(0)) => (lines.next() == Some("yes"), lines.next().unwrap_or_default().to_string()),
            _ => (false, String::new()),
        };
        let path =
            if path.is_empty() { "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string() } else { path };
        self.container = Some(Container { name, has_bash, path });
        self.log.line("##[endgroup]");
        Ok(())
    }

    /// Remove the job container and any processes the job left behind.
    async fn cleanup(&mut self) {
        if let Some(c) = self.container.take() {
            #[cfg(unix)]
            {
                // Files created as root in the container would otherwise be
                // impossible to delete from the workspace.
                // SAFETY: getuid/getgid have no preconditions.
                let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
                let chown = format!("chown -R {uid}:{gid} {C_WORKSPACE} {C_TEMP} 2>/dev/null || true");
                let mut cmd = tokio::process::Command::new("docker");
                cmd.args(["exec", &c.name, "sh", "-c", &chown])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null());
                let _ = tokio::time::timeout(Duration::from_secs(120), cmd.status()).await;
            }
            let mut rm = tokio::process::Command::new("docker");
            rm.args(["rm", "-f", &c.name]).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
            if !matches!(tokio::time::timeout(Duration::from_secs(60), rm.status()).await, Ok(Ok(s)) if s.success()) {
                tracing::warn!(container = c.name, "could not remove the job container");
            }
        }
        self.exec.kill_all().await;
    }
}

fn add_shell_args(cmd: &mut std::process::Command, sc: &shell::ShellCommand) {
    // cmd.exe parses its own command line; pass it verbatim.
    #[cfg(windows)]
    if sc.kind == shell::Kind::Cmd {
        use std::os::windows::process::CommandExt;
        cmd.raw_arg(sc.args.join(" "));
        return;
    }
    cmd.args(&sc.args);
}
