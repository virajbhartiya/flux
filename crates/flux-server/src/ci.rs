//! Built-in CI: GitHub Actions–style workflows run by `flux-runner`.
//!
//! Workflows live in `.flux-ci/workflows/*.yml` or `.github/workflows/*.yml`.
//! The supported subset covers most real pipelines:
//! * triggers `push`, `pull_request` (flux reviews) and `workflow_dispatch`,
//!   with `branches`, `branches-ignore`, `tags`, `tags-ignore`, `paths` and
//!   `paths-ignore` filters;
//! * `jobs.<id>`: `name`, `runs-on`, `needs`, `if`, `env`, `timeout-minutes`,
//!   `container`, `defaults.run`, `strategy.matrix` (with `include` and
//!   `exclude`), `continue-on-error`;
//! * steps with `run`, `shell`, `working-directory`, `env`, `if`, `id`,
//!   `continue-on-error`, `timeout-minutes`, and `uses: actions/checkout`
//!   (native). Other actions run when the runner has a handler for them.
//!
//! The server parses workflows, expands matrices into jobs, queues them and
//! tracks `needs`; runners claim jobs (leased, so a dead runner's job is
//! picked up again), stream logs, and report results. Every job reports a
//! commit status `<workflow> / <job>`, so branch protection can require it.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use flux_core::ChangesetId;
use flux_store::{CiJob, CiRun, NewCiJob, RepoId};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::auth::{Principal, now_ms};
use crate::core::{Event, Flux, FluxError, FluxResult};
use crate::rbac::Role;

const WORKFLOW_DIRS: [&str; 2] = [".flux-ci/workflows", ".github/workflows"];
const MAX_JOBS: usize = 256;
/// How long a claimed job stays with its runner without a heartbeat.
pub const LEASE_MS: i64 = 90_000;

/// A job as handed to a runner.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobSpec {
    pub workflow: String,
    pub job: String,
    pub name: String,
    #[serde(default)]
    pub r#if: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub matrix: BTreeMap<String, Value>,
    pub timeout_minutes: u64,
    #[serde(default)]
    pub container: Option<String>,
    #[serde(default)]
    pub defaults: Defaults,
    pub steps: Vec<StepSpec>,
    #[serde(default)]
    pub continue_on_error: bool,
    /// `github.*` context values.
    #[serde(default)]
    pub context: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Defaults {
    #[serde(default)]
    pub shell: Option<String>,
    #[serde(default)]
    pub working_directory: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepSpec {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub run: Option<String>,
    #[serde(default)]
    pub uses: Option<String>,
    #[serde(default)]
    pub with: BTreeMap<String, String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub r#if: Option<String>,
    #[serde(default)]
    pub shell: Option<String>,
    #[serde(default)]
    pub working_directory: Option<String>,
    #[serde(default)]
    pub continue_on_error: bool,
    #[serde(default)]
    pub timeout_minutes: Option<u64>,
}

/// What triggered a run.
#[derive(Debug, Clone)]
pub struct Trigger {
    /// `push`, `pull_request` or `workflow_dispatch`.
    pub event: String,
    /// Branch or tag (`main`, `tags/v1`); for reviews, the target branch.
    pub ref_name: String,
    pub cs: ChangesetId,
    pub before: Option<ChangesetId>,
    pub review: Option<i64>,
    pub actor: String,
    /// Restrict to one workflow file (manual dispatch).
    pub only: Option<String>,
}

fn scalar(v: &serde_yaml::Value) -> Option<String> {
    match v {
        serde_yaml::Value::String(s) => Some(s.clone()),
        serde_yaml::Value::Bool(b) => Some(b.to_string()),
        serde_yaml::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn str_map(v: Option<&serde_yaml::Value>) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    if let Some(serde_yaml::Value::Mapping(m)) = v {
        for (k, v) in m {
            if let (Some(k), Some(v)) = (scalar(k), scalar(v)) {
                out.insert(k, v);
            }
        }
    }
    out
}

fn str_list(v: Option<&serde_yaml::Value>) -> Vec<String> {
    match v {
        Some(serde_yaml::Value::Sequence(s)) => s.iter().filter_map(scalar).collect(),
        Some(v) => scalar(v).into_iter().collect(),
        None => vec![],
    }
}

fn get<'a>(v: &'a serde_yaml::Value, key: &str) -> Option<&'a serde_yaml::Value> {
    v.as_mapping().and_then(|m| m.get(serde_yaml::Value::String(key.into())))
}

fn yaml_to_json(v: &serde_yaml::Value) -> Value {
    serde_json::to_value(v).unwrap_or(Value::Null)
}

/// Substitute `${{ matrix.x }}` in a string (other expressions are left
/// for the runner).
fn subst_matrix(s: &str, matrix: &BTreeMap<String, Value>) -> String {
    let mut out = s.to_string();
    for (k, v) in matrix {
        let val = match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        for pat in [format!("${{{{ matrix.{k} }}}}"), format!("${{{{matrix.{k}}}}}")] {
            out = out.replace(&pat, &val);
        }
    }
    out
}

/// Expand `strategy.matrix` into combinations.
fn expand_matrix(m: Option<&serde_yaml::Value>) -> Vec<BTreeMap<String, Value>> {
    let Some(serde_yaml::Value::Mapping(map)) = m else { return vec![BTreeMap::new()] };
    let mut combos: Vec<BTreeMap<String, Value>> = vec![BTreeMap::new()];
    let mut include = vec![];
    let mut exclude = vec![];
    for (k, v) in map {
        let Some(key) = scalar(k) else { continue };
        match (key.as_str(), v) {
            ("include", serde_yaml::Value::Sequence(s)) => include = s.clone(),
            ("exclude", serde_yaml::Value::Sequence(s)) => exclude = s.clone(),
            (_, serde_yaml::Value::Sequence(vals)) => {
                let mut next = Vec::new();
                for c in &combos {
                    for val in vals {
                        let mut c = c.clone();
                        c.insert(key.clone(), yaml_to_json(val));
                        next.push(c);
                    }
                }
                combos = next;
            }
            _ => {}
        }
        if combos.len() > MAX_JOBS {
            combos.truncate(MAX_JOBS);
        }
    }
    let matches = |c: &BTreeMap<String, Value>, e: &serde_yaml::Value| {
        e.as_mapping().is_some_and(|m| m.iter().all(|(k, v)| scalar(k).is_some_and(|k| c.get(&k) == Some(&yaml_to_json(v)))))
    };
    combos.retain(|c| !exclude.iter().any(|e| matches(c, e)));
    let base_keys: BTreeSet<String> = combos.first().map(|c| c.keys().cloned().collect()).unwrap_or_default();
    for inc in include {
        let Some(m) = inc.as_mapping() else { continue };
        let entry: BTreeMap<String, Value> = m.iter().filter_map(|(k, v)| Some((scalar(k)?, yaml_to_json(v)))).collect();
        let mut extended = false;
        for c in combos.iter_mut() {
            let compatible = entry.iter().filter(|(k, _)| base_keys.contains(*k)).all(|(k, v)| c.get(k) == Some(v));
            let has_base = entry.keys().any(|k| base_keys.contains(k));
            if compatible && has_base {
                for (k, v) in &entry {
                    c.entry(k.clone()).or_insert(v.clone());
                }
                extended = true;
            }
        }
        if !extended {
            if combos.len() == 1 && combos[0].is_empty() {
                combos[0] = entry;
            } else {
                combos.push(entry);
            }
        }
    }
    if combos.is_empty() {
        // Everything excluded: no jobs.
        return vec![];
    }
    combos
}

/// Does a trigger's filter object accept this ref and these paths?
fn filter_accepts(filter: Option<&serde_yaml::Value>, t: &Trigger, paths: &[String]) -> bool {
    let Some(f) = filter.filter(|f| f.is_mapping()) else { return true };
    let (is_tag, name) = match t.ref_name.strip_prefix("tags/") {
        Some(tag) => (true, tag.to_string()),
        None => (false, t.ref_name.clone()),
    };
    let any = |pats: &[String], s: &str| pats.iter().any(|p| crate::protect::ref_matches(p, s) || p == s);
    let branches = str_list(get(f, "branches"));
    let branches_ignore = str_list(get(f, "branches-ignore"));
    let tags = str_list(get(f, "tags"));
    let tags_ignore = str_list(get(f, "tags-ignore"));
    if is_tag {
        // A push filter with only branch patterns does not run on tags.
        if tags.is_empty() && tags_ignore.is_empty() && (!branches.is_empty() || !branches_ignore.is_empty()) {
            return false;
        }
        if !tags.is_empty() && !any(&tags, &name) {
            return false;
        }
        if any(&tags_ignore, &name) {
            return false;
        }
    } else {
        if branches.is_empty() && branches_ignore.is_empty() && !tags.is_empty() {
            return false;
        }
        if !branches.is_empty() && !any(&branches, &name) {
            return false;
        }
        if any(&branches_ignore, &name) {
            return false;
        }
    }
    let inc = str_list(get(f, "paths"));
    let exc = str_list(get(f, "paths-ignore"));
    if !inc.is_empty() && !paths.iter().any(|p| inc.iter().any(|g| path_glob(g, p))) {
        return false;
    }
    if !exc.is_empty() && paths.iter().all(|p| exc.iter().any(|g| path_glob(g, p))) && !paths.is_empty() {
        return false;
    }
    true
}

fn path_glob(pattern: &str, path: &str) -> bool {
    crate::protect::ref_matches(pattern, path) || (pattern.ends_with("/**") && path.starts_with(pattern.trim_end_matches("**")))
}

/// Parse one workflow file into jobs for a trigger (`None` if it does not
/// run).
pub fn plan(
    file: &str,
    text: &str,
    t: &Trigger,
    paths: &[String],
) -> Result<Option<(String, Vec<(JobSpec, Vec<String>, Vec<String>)>)>, String> {
    let doc: serde_yaml::Value = serde_yaml::from_str(text).map_err(|e| format!("{file}: {e}"))?;
    let name = get(&doc, "name").and_then(scalar).unwrap_or_else(|| {
        file.rsplit('/').next().unwrap_or(file).trim_end_matches(".yml").trim_end_matches(".yaml").to_string()
    });
    // `on` parses as the boolean `true` in YAML 1.1.
    let on = get(&doc, "on").or_else(|| doc.as_mapping().and_then(|m| m.get(serde_yaml::Value::Bool(true))));
    let event_filter: Option<Option<&serde_yaml::Value>> = match on {
        Some(serde_yaml::Value::String(s)) => (s == &t.event).then_some(None),
        Some(serde_yaml::Value::Sequence(v)) => v.iter().any(|e| scalar(e).as_deref() == Some(t.event.as_str())).then_some(None),
        Some(serde_yaml::Value::Mapping(m)) => m.get(serde_yaml::Value::String(t.event.clone())).map(Some),
        _ => None,
    };
    let Some(filter) = event_filter else { return Ok(None) };
    if t.event != "workflow_dispatch" && !filter_accepts(filter, t, paths) {
        return Ok(None);
    }
    let wf_env = str_map(get(&doc, "env"));
    let wf_defaults = get(&doc, "defaults").and_then(|d| get(d, "run"));
    let Some(serde_yaml::Value::Mapping(jobs)) = get(&doc, "jobs") else {
        return Err(format!("{file}: no jobs"));
    };
    let mut out = Vec::new();
    for (id, job) in jobs {
        let Some(id) = scalar(id) else { continue };
        let needs = str_list(get(job, "needs"));
        let steps_yaml = match get(job, "steps") {
            Some(serde_yaml::Value::Sequence(s)) => s.clone(),
            _ => return Err(format!("{file}: job {id} has no steps")),
        };
        let defaults = get(job, "defaults").and_then(|d| get(d, "run")).or(wf_defaults);
        for matrix in expand_matrix(get(job, "strategy").and_then(|s| get(s, "matrix"))) {
            let display = match get(job, "name").and_then(scalar) {
                Some(n) => subst_matrix(&n, &matrix),
                None if matrix.is_empty() => id.clone(),
                None => format!(
                    "{id} ({})",
                    matrix
                        .values()
                        .map(|v| match v {
                            Value::String(s) => s.clone(),
                            o => o.to_string(),
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            };
            let mut env = wf_env.clone();
            env.extend(str_map(get(job, "env")).into_iter().map(|(k, v)| (k, subst_matrix(&v, &matrix))));
            let labels: Vec<String> = str_list(get(job, "runs-on")).iter().map(|l| subst_matrix(l, &matrix)).collect();
            let container = get(job, "container").and_then(|c| scalar(c).or_else(|| get(c, "image").and_then(scalar)));
            let steps = steps_yaml
                .iter()
                .map(|s| StepSpec {
                    id: get(s, "id").and_then(scalar),
                    name: get(s, "name").and_then(scalar).map(|n| subst_matrix(&n, &matrix)),
                    run: get(s, "run").and_then(scalar).map(|r| subst_matrix(&r, &matrix)),
                    uses: get(s, "uses").and_then(scalar),
                    with: str_map(get(s, "with")).into_iter().map(|(k, v)| (k, subst_matrix(&v, &matrix))).collect(),
                    env: str_map(get(s, "env")).into_iter().map(|(k, v)| (k, subst_matrix(&v, &matrix))).collect(),
                    r#if: get(s, "if").and_then(scalar),
                    shell: get(s, "shell").and_then(scalar),
                    working_directory: get(s, "working-directory").and_then(scalar),
                    continue_on_error: get(s, "continue-on-error").and_then(|v| v.as_bool()).unwrap_or(false),
                    timeout_minutes: get(s, "timeout-minutes").and_then(|v| v.as_u64()),
                })
                .collect();
            let spec = JobSpec {
                workflow: name.clone(),
                job: id.clone(),
                name: display,
                r#if: get(job, "if").and_then(scalar),
                env,
                matrix: matrix.clone(),
                timeout_minutes: get(job, "timeout-minutes").and_then(|v| v.as_u64()).unwrap_or(360).min(24 * 60),
                container: container.map(|c| subst_matrix(&c, &matrix)),
                defaults: Defaults {
                    shell: defaults.and_then(|d| get(d, "shell")).and_then(scalar),
                    working_directory: defaults.and_then(|d| get(d, "working-directory")).and_then(scalar),
                },
                steps,
                continue_on_error: get(job, "continue-on-error").and_then(|v| v.as_bool()).unwrap_or(false),
                context: BTreeMap::new(),
            };
            out.push((spec, needs.clone(), labels));
            if out.len() > MAX_JOBS {
                return Err(format!("{file}: more than {MAX_JOBS} jobs"));
            }
        }
    }
    Ok(Some((name, out)))
}

pub fn status_context(spec: &JobSpec) -> String {
    format!("{} / {}", spec.workflow, spec.name)
}

/// Runner-facing view of a claimed job.
#[derive(Debug, Clone, Serialize)]
pub struct ClaimedJob {
    pub id: i64,
    pub run_id: i64,
    pub repo: String,
    pub clone_url: String,
    pub changeset: String,
    pub git_sha: String,
    /// A read token for this repository, valid while the job runs.
    pub token: String,
    pub spec: JobSpec,
    pub secrets: BTreeMap<String, String>,
}

fn terminal(state: &str) -> bool {
    matches!(state, "success" | "failure" | "cancelled" | "skipped")
}

impl Flux {
    /// Workflow files at a changeset: (path, text).
    pub async fn workflow_files(&self, repo: RepoId, cs: &ChangesetId) -> FluxResult<Vec<(String, String)>> {
        let mut out = Vec::new();
        for dir in WORKFLOW_DIRS {
            let path = flux_core::RepoPath::new(dir.as_bytes().to_vec())?;
            let Some(e) = self.entry_at(repo, cs, &path).await? else { continue };
            if !e.is_dir() {
                continue;
            }
            let root = self.root_tree(repo, cs).await?;
            let files = self.file_list(root).await?;
            for (p, c, size) in files.iter() {
                let p = String::from_utf8_lossy(p.as_bytes()).to_string();
                let Some(rest) = p.strip_prefix(&format!("{dir}/")) else { continue };
                if rest.contains('/') || !(rest.ends_with(".yml") || rest.ends_with(".yaml")) || *size > 512 * 1024 {
                    continue;
                }
                if let Some(data) = self.blobs.get_content_bytes(c).await? {
                    out.push((p, String::from_utf8_lossy(&data).into_owned()));
                }
            }
            // The first directory with workflows wins (no double runs).
            if !out.is_empty() {
                break;
            }
        }
        Ok(out)
    }

    async fn trigger_paths(&self, repo: RepoId, t: &Trigger) -> FluxResult<Vec<String>> {
        let old = match (t.before, t.review) {
            (Some(b), _) => Some(self.root_tree(repo, &b).await?),
            (None, Some(n)) => {
                let r = self.review(repo, n).await?;
                match self.meta.get_ref(repo, &r.target).await? {
                    Some(h) => match self.merge_base(repo, &h.target, &t.cs).await? {
                        Some(b) => Some(self.root_tree(repo, &b).await?),
                        None => None,
                    },
                    None => None,
                }
            }
            _ => None,
        };
        let new = self.root_tree(repo, &t.cs).await?;
        let diff = flux_derive::diff_trees(&self.blobs, old, Some(new), &|_, _| true).await?;
        Ok(diff.into_iter().take(10_000).map(|d| d.path.to_string()).collect())
    }

    /// Create runs for every workflow the trigger matches. Returns run ids.
    pub async fn trigger_ci(self: &Arc<Self>, repo: RepoId, t: Trigger) -> FluxResult<Vec<i64>> {
        let files = self.workflow_files(repo, &t.cs).await?;
        if files.is_empty() {
            return Ok(vec![]);
        }
        let paths = self.trigger_paths(repo, &t).await.unwrap_or_default();
        let repo_name = self.repo_name(repo).await;
        let base = self.settings.base_url();
        let git_sha = hex::encode(self.git_commit_shas(repo, &[t.cs]).await?.first().copied().unwrap_or([0; 20]));
        let mut runs = Vec::new();
        for (file, text) in files {
            if t.only.as_ref().is_some_and(|o| !file.ends_with(o.as_str())) {
                continue;
            }
            let planned = match plan(&file, &text, &t, &paths) {
                Ok(Some(p)) => p,
                Ok(None) => continue,
                Err(e) => {
                    // A broken workflow is a failed check, not a silent no-op.
                    let _ = self.set_status(repo, t.cs, "error", &format!("workflow {file}"), &e, "", "flux-ci").await;
                    continue;
                }
            };
            let (wf_name, jobs) = planned;
            if jobs.is_empty() {
                continue;
            }
            let (git_ref, ref_name) = match t.ref_name.strip_prefix("tags/") {
                Some(tag) => (format!("refs/tags/{tag}"), tag.to_string()),
                None => (format!("refs/heads/{}", t.ref_name), t.ref_name.clone()),
            };
            let new_jobs: Vec<NewCiJob> = jobs
                .iter()
                .map(|(spec, needs, labels)| {
                    let mut spec = spec.clone();
                    spec.context = BTreeMap::from([
                        ("repository".to_string(), json!(repo_name)),
                        ("ref".to_string(), json!(git_ref)),
                        ("ref_name".to_string(), json!(ref_name)),
                        ("sha".to_string(), json!(git_sha)),
                        ("changeset".to_string(), json!(t.cs.to_hex())),
                        ("event_name".to_string(), json!(t.event)),
                        ("actor".to_string(), json!(t.actor)),
                        ("server_url".to_string(), json!(base)),
                        ("workflow".to_string(), json!(wf_name)),
                        ("job".to_string(), json!(spec.job)),
                        ("review".to_string(), json!(t.review)),
                    ]);
                    // `needs` name job ids; matrix jobs share their id.
                    NewCiJob {
                        name: spec.job.clone(),
                        needs: needs.clone(),
                        labels: labels.clone(),
                        spec: serde_json::to_string(&spec).unwrap_or_default(),
                    }
                })
                .collect();
            let run = CiRun {
                id: 0,
                repo_id: repo,
                workflow: wf_name.clone(),
                event: t.event.clone(),
                ref_name: t.ref_name.clone(),
                cs: t.cs,
                review: t.review,
                state: "queued".into(),
                actor: t.actor.clone(),
                created_ms: 0,
                updated_ms: 0,
            };
            let (run, created) = self.meta.create_ci_run(&run, &new_jobs).await?;
            for j in &created {
                if let Ok(spec) = serde_json::from_str::<JobSpec>(&j.spec) {
                    let url = format!("{base}/{repo_name}/-/runs/{}", run.id);
                    let _ = self.set_status(repo, t.cs, "pending", &status_context(&spec), "Queued", &url, "flux-ci").await;
                }
            }
            self.publish(Event::Ci { repo: repo_name.clone(), run: run.id, state: "queued".into() });
            runs.push(run.id);
        }
        if !runs.is_empty() {
            self.ci_wake.notify_waiters();
        }
        Ok(runs)
    }

    /// Manually start a workflow (`workflow_dispatch`).
    pub async fn dispatch_workflow(
        self: &Arc<Self>,
        who: &Principal,
        repo: RepoId,
        workflow: &str,
        rev: &str,
    ) -> FluxResult<Vec<i64>> {
        self.authorize(who, repo, Role::Write).await?;
        let cs = self.resolve_rev(repo, rev).await?;
        let ref_name = if self.meta.get_ref(repo, rev.as_bytes()).await?.is_some() { rev.to_string() } else { String::new() };
        self.trigger_ci(
            repo,
            Trigger {
                event: "workflow_dispatch".into(),
                ref_name,
                cs,
                before: None,
                review: None,
                actor: who.name().into(),
                only: Some(workflow.to_string()),
            },
        )
        .await
    }

    /// A runner asks for work: claim the oldest job it can run.
    pub async fn claim_job(&self, runner: &str, labels: &[String]) -> FluxResult<Option<ClaimedJob>> {
        let now = now_ms();
        let Some(job) = self.meta.claim_ci_job(runner, labels, now, now + LEASE_MS).await? else { return Ok(None) };
        let spec: JobSpec =
            serde_json::from_str(&job.spec).map_err(|e| FluxError::Internal(anyhow::anyhow!("bad job spec: {e}")))?;
        let run = self.meta.get_ci_run(job.run_id).await?.ok_or_else(|| FluxError::NotFound("run".into()))?;
        let repo_name = self.repo_name(job.repo_id).await;
        if run.state == "queued" {
            self.meta.set_ci_run_state(run.id, "running").await?;
            self.publish(Event::Ci { repo: repo_name.clone(), run: run.id, state: "running".into() });
        }
        let base = self.settings.base_url();
        let url = format!("{base}/{repo_name}/-/runs/{}", run.id);
        let _ = self.set_status(job.repo_id, run.cs, "pending", &status_context(&spec), "Running", &url, "flux-ci").await;
        let token = self.job_token(job.repo_id, job.id).await?;
        let secrets = self.job_secrets(job.repo_id).await?;
        Ok(Some(ClaimedJob {
            id: job.id,
            run_id: run.id,
            repo: repo_name.clone(),
            clone_url: format!("{base}/{repo_name}.git"),
            changeset: run.cs.to_hex(),
            git_sha: spec.context.get("sha").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
            token,
            spec,
            secrets,
        }))
    }

    /// A short-lived token that can read only this repository.
    async fn job_token(&self, repo: RepoId, job: i64) -> FluxResult<String> {
        let bot = match self.meta.get_user("flux-ci").await? {
            Some(u) => u,
            None => {
                let u = self.meta.create_user("flux-ci", false).await?;
                self.meta.set_user_kind(u.id, "bot").await?;
                u
            }
        };
        let token = crate::auth::new_token();
        self.meta
            .add_token_ext(
                bot.id,
                &crate::auth::hash_token(&token),
                &format!("ci job {job}"),
                "api",
                &format!("read repo:{}", repo.0),
                Some(now_ms() + 24 * 3_600_000),
            )
            .await?;
        Ok(token)
    }

    /// Heartbeat and log upload. Returns whether the job was cancelled.
    pub async fn job_log(&self, runner: &str, job_id: i64, seq: i64, data: &[u8]) -> FluxResult<bool> {
        let job = self.runner_job(runner, job_id).await?;
        if job.state == "cancelled" {
            return Ok(true);
        }
        if !data.is_empty() {
            self.meta.append_ci_log(job_id, seq, data).await?;
        }
        self.meta.set_ci_job(job_id, runner, "running", None, now_ms() + LEASE_MS, now_ms()).await?;
        Ok(false)
    }

    async fn runner_job(&self, runner: &str, job_id: i64) -> FluxResult<CiJob> {
        let job = self.meta.get_ci_job(job_id).await?.ok_or_else(|| FluxError::NotFound("job".into()))?;
        if job.runner != runner {
            return Err(FluxError::PermissionDenied("this job belongs to another runner".into()));
        }
        Ok(job)
    }

    /// A runner reports a job's result.
    pub async fn finish_job(
        self: &Arc<Self>,
        runner: &str,
        job_id: i64,
        conclusion: &str,
        exit_code: Option<i64>,
    ) -> FluxResult<()> {
        if !matches!(conclusion, "success" | "failure" | "cancelled" | "skipped") {
            return Err(FluxError::InvalidArgument("conclusion: success | failure | cancelled | skipped".into()));
        }
        let job = self.runner_job(runner, job_id).await?;
        if terminal(&job.state) {
            return Ok(());
        }
        self.meta.set_ci_job(job_id, runner, conclusion, exit_code, 0, now_ms()).await?;
        self.job_finished(&job, conclusion).await
    }

    async fn job_finished(self: &Arc<Self>, job: &CiJob, conclusion: &str) -> FluxResult<()> {
        let run = self.meta.get_ci_run(job.run_id).await?.ok_or_else(|| FluxError::NotFound("run".into()))?;
        if let Ok(spec) = serde_json::from_str::<JobSpec>(&job.spec) {
            let (state, desc) = match conclusion {
                "success" => ("success", "Succeeded"),
                "skipped" => ("success", "Skipped"),
                "cancelled" => ("error", "Cancelled"),
                _ => ("failure", "Failed"),
            };
            let url = format!("{}/{}/-/runs/{}", self.settings.base_url(), self.repo_name(job.repo_id).await, run.id);
            let _ = self.set_status(job.repo_id, run.cs, state, &status_context(&spec), desc, &url, "flux-ci").await;
        }
        self.advance_run(&run).await
    }

    /// Unblock jobs whose needs finished; settle the run when all are done.
    async fn advance_run(self: &Arc<Self>, run: &CiRun) -> FluxResult<()> {
        let now = now_ms();
        loop {
            let jobs = self.meta.ci_jobs(run.id).await?;
            let mut by_name: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
            for j in &jobs {
                by_name.entry(&j.name).or_default().push(&j.state);
            }
            let mut changed = false;
            for j in jobs.iter().filter(|j| j.state == "blocked") {
                let states: Vec<&str> =
                    j.needs.iter().flat_map(|n| by_name.get(n.as_str()).cloned().unwrap_or_default()).collect();
                if states.iter().any(|s| !terminal(s)) {
                    continue;
                }
                let cond = serde_json::from_str::<JobSpec>(&j.spec).ok().and_then(|s| s.r#if).unwrap_or_default();
                let needs_ok = states.iter().all(|s| matches!(*s, "success" | "skipped"));
                let runs_anyway = cond.contains("always()") || (!needs_ok && cond.contains("failure()"));
                if needs_ok || runs_anyway {
                    self.meta.set_ci_job(j.id, "", "queued", None, 0, now).await?;
                    self.ci_wake.notify_waiters();
                } else {
                    self.meta.set_ci_job(j.id, "", "skipped", None, 0, now).await?;
                    if let Ok(spec) = serde_json::from_str::<JobSpec>(&j.spec) {
                        let _ = self
                            .set_status(
                                j.repo_id,
                                run.cs,
                                "failure",
                                &status_context(&spec),
                                "Skipped: a needed job failed",
                                "",
                                "flux-ci",
                            )
                            .await;
                    }
                }
                changed = true;
            }
            if !changed {
                break;
            }
        }
        let jobs = self.meta.ci_jobs(run.id).await?;
        if jobs.iter().all(|j| terminal(&j.state)) {
            let state = if jobs.iter().any(|j| j.state == "cancelled") {
                "cancelled"
            } else if jobs.iter().any(|j| j.state == "failure") {
                "failure"
            } else if jobs.iter().all(|j| j.state == "skipped")
                || jobs.iter().all(|j| matches!(j.state.as_str(), "success" | "skipped"))
            {
                "success"
            } else {
                "failure"
            };
            self.meta.set_ci_run_state(run.id, state).await?;
            let name = self.repo_name(run.repo_id).await;
            self.publish(Event::Ci { repo: name, run: run.id, state: state.into() });
        }
        Ok(())
    }

    pub async fn cancel_run(self: &Arc<Self>, who: &Principal, repo: RepoId, run_id: i64) -> FluxResult<()> {
        self.authorize(who, repo, Role::Write).await?;
        let run =
            self.meta.get_ci_run(run_id).await?.filter(|r| r.repo_id == repo).ok_or_else(|| FluxError::NotFound("run".into()))?;
        for j in self.meta.ci_jobs(run_id).await? {
            if !terminal(&j.state) {
                self.meta.set_ci_job(j.id, "", "cancelled", None, 0, now_ms()).await?;
                if let Ok(spec) = serde_json::from_str::<JobSpec>(&j.spec) {
                    let _ = self.set_status(repo, run.cs, "error", &status_context(&spec), "Cancelled", "", "flux-ci").await;
                }
            }
        }
        self.advance_run(&run).await
    }

    /// Run a run's workflow again from scratch.
    pub async fn rerun(self: &Arc<Self>, who: &Principal, repo: RepoId, run_id: i64) -> FluxResult<Vec<i64>> {
        self.authorize(who, repo, Role::Write).await?;
        let run =
            self.meta.get_ci_run(run_id).await?.filter(|r| r.repo_id == repo).ok_or_else(|| FluxError::NotFound("run".into()))?;
        let files = self.workflow_files(repo, &run.cs).await?;
        let mut only = None;
        for (f, text) in &files {
            if serde_yaml::from_str::<serde_yaml::Value>(text).ok().and_then(|d| get(&d, "name").and_then(scalar)).as_deref()
                == Some(run.workflow.as_str())
                || f.ends_with(&format!("/{}.yml", run.workflow))
                || f.ends_with(&format!("/{}.yaml", run.workflow))
            {
                only = Some(f.clone());
            }
        }
        // Re-planning with the original event keeps filters meaningful.
        self.trigger_ci(
            repo,
            Trigger {
                event: if run.event == "workflow_dispatch" { "workflow_dispatch".into() } else { run.event.clone() },
                ref_name: run.ref_name.clone(),
                cs: run.cs,
                before: None,
                review: run.review,
                actor: who.name().into(),
                only,
            },
        )
        .await
    }
}

/// React to pushes and reviews.
pub(crate) async fn on_event(flux: &Arc<Flux>, repo: RepoId, e: &Event) -> FluxResult<()> {
    let t = match e {
        Event::RefMoved { name, old, new: Some(new), actor, .. } => {
            let n = String::from_utf8_lossy(name).to_string();
            if n.starts_with("pull/") || n.starts_with("review/") || n.starts_with("flux/") {
                return Ok(());
            }
            if old.is_none() && flux.ci_quiet_tags.remove(&(repo.0, name.clone())) == Some(*new) {
                return Ok(());
            }
            Trigger { event: "push".into(), ref_name: n, cs: *new, before: *old, review: None, actor: actor.clone(), only: None }
        }
        Event::Review { number, kind, actor, .. } if matches!(kind.as_str(), "opened" | "updated" | "reopened" | "ready") => {
            let r = flux.review(repo, *number).await?;
            if flux.meta.review_flags(repo, *number).await?.0 && kind != "ready" {
                // Drafts run CI too (like GitHub), so authors see results early.
            }
            Trigger {
                event: "pull_request".into(),
                ref_name: String::from_utf8_lossy(&r.target).into(),
                cs: r.head,
                before: None,
                review: Some(*number),
                actor: actor.clone(),
                only: None,
            }
        }
        _ => return Ok(()),
    };
    flux.trigger_ci(repo, t).await?;
    Ok(())
}

/// Periodic housekeeping: jobs whose runner vanished are re-queued when
/// their lease expires (by `claim`); runs stuck with only expired jobs for a
/// long time are failed so they do not block protection forever.
pub(crate) fn start_scheduler(flux: Arc<Flux>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            // Wake long-polling runners so expired leases get re-claimed.
            flux.ci_wake.notify_waiters();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(event: &str, r: &str) -> Trigger {
        Trigger {
            event: event.into(),
            ref_name: r.into(),
            cs: ChangesetId::hash(b"x"),
            before: None,
            review: None,
            actor: "a".into(),
            only: None,
        }
    }

    const WF: &str = r#"
name: CI
on:
  push:
    branches: [main, "release/*"]
    paths-ignore: ["docs/**"]
  pull_request:
env:
  RUST: stable
jobs:
  test:
    runs-on: ${{ matrix.os }}
    strategy:
      matrix:
        os: [linux, macos]
        rust: [stable, beta]
        exclude:
          - os: macos
            rust: beta
        include:
          - os: linux
            rust: nightly
            experimental: true
    steps:
      - uses: actions/checkout@v4
      - run: cargo test --${{ matrix.rust }}
  deploy:
    needs: test
    if: github.ref == 'refs/heads/main'
    runs-on: linux
    steps:
      - run: ./deploy.sh
"#;

    #[test]
    fn plans_matrix_and_filters() {
        let (name, jobs) = plan("ci.yml", WF, &t("push", "main"), &["src/lib.rs".into()]).unwrap().unwrap();
        assert_eq!(name, "CI");
        let names: Vec<_> = jobs.iter().map(|(s, _, _)| s.name.clone()).collect();
        assert_eq!(
            names,
            vec!["test (linux, stable)", "test (linux, beta)", "test (macos, stable)", "test (true, linux, nightly)", "deploy"]
        );
        assert_eq!(jobs[2].2, vec!["macos"]);
        assert_eq!(jobs[0].0.steps[1].run.as_deref(), Some("cargo test --stable"));
        assert_eq!(jobs[4].1, vec!["test"]);
        assert_eq!(jobs[0].0.env.get("RUST").map(String::as_str), Some("stable"));
        // Branch, path and event filters.
        assert!(plan("ci.yml", WF, &t("push", "feature"), &["src/lib.rs".into()]).unwrap().is_none());
        assert!(plan("ci.yml", WF, &t("push", "release/1.0"), &["src/lib.rs".into()]).unwrap().is_some());
        assert!(plan("ci.yml", WF, &t("push", "main"), &["docs/a.md".into()]).unwrap().is_none());
        assert!(plan("ci.yml", WF, &t("push", "tags/v1"), &[]).unwrap().is_none());
        assert!(plan("ci.yml", WF, &t("pull_request", "main"), &[]).unwrap().is_some());
        assert!(plan("ci.yml", WF, &t("workflow_dispatch", "main"), &[]).unwrap().is_none());
        assert!(plan("x.yml", "on: [push]\njobs: {}", &t("push", "main"), &[]).unwrap().unwrap().1.is_empty());
        assert!(plan("x.yml", "on: push\njobs:\n  a:\n    runs-on: x\n", &t("push", "main"), &[]).is_err());
    }
}
