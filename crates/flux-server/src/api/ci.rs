//! CI endpoints: runner registration, the runner protocol, runs and logs.
//!
//! Runner protocol (authenticated with a runner token, `flxr_…`):
//! * `POST /api/v1/ci/claim` `{labels}` → a job, or 204 after a long poll;
//! * `POST /api/v1/ci/jobs/{id}/log?seq=N` (raw bytes) → `{cancelled}`,
//!   also a heartbeat that keeps the lease;
//! * `POST /api/v1/ci/jobs/{id}/finish` `{conclusion, exit_code}`.

use axum::http::{Method, StatusCode, header};
use axum::response::IntoResponse;
use flux_store::RepoId;
use serde::Deserialize;
use serde_json::json;

use super::{ApiError, ApiResult, Ctx, bad, created, no_content, not_found, reply};
use crate::rbac::Role;

fn num(s: &str) -> ApiResult<i64> {
    s.parse().map_err(|_| bad(format!("{s:?} is not a number")))
}

/// Admin: list, register and remove runners.
pub async fn runners(c: &Ctx, t: &[&str]) -> ApiResult {
    c.who.require_admin()?;
    match (t, &c.method) {
        ([], &Method::GET) => {
            let list = c.flux.meta.list_ci_runners().await?;
            reply(list.iter().map(|r| json!({ "id": r.id, "name": r.name, "labels": r.labels, "created_at": r.created_ms, "last_seen": r.last_seen })).collect::<Vec<_>>())
        }
        ([], &Method::POST) => {
            #[derive(Deserialize)]
            struct B {
                name: String,
                #[serde(default)]
                labels: Vec<String>,
            }
            let b: B = c.parse()?;
            if b.name.trim().is_empty() {
                return Err(bad("a runner needs a name"));
            }
            let mut raw = [0u8; 32];
            getrandom::fill(&mut raw).expect("OS random number generator");
            let token = format!("flxr_{}", hex::encode(raw));
            let r = c.flux.meta.create_ci_runner(b.name.trim(), &crate::auth::hash_token(&token), &b.labels).await?;
            c.flux.audit(&c.who, "runner.create", &r.name, &b.labels.join(" ")).await;
            created(json!({ "id": r.id, "name": r.name, "labels": r.labels, "token": token }))
        }
        ([id], &Method::DELETE) => {
            if !c.flux.meta.delete_ci_runner(num(id)?).await? {
                return Err(not_found("runner"));
            }
            no_content()
        }
        _ => Err(not_found("no such endpoint")),
    }
}

async fn runner_name(c: &Ctx) -> ApiResult<String> {
    let token = c
        .headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer ").or_else(|| v.strip_prefix("Runner ")))
        .ok_or_else(|| ApiError(StatusCode::UNAUTHORIZED, "runner token required".into()))?;
    let r = c
        .flux
        .meta
        .ci_runner_by_token(&crate::auth::hash_token(token), crate::auth::now_ms())
        .await?
        .ok_or_else(|| ApiError(StatusCode::UNAUTHORIZED, "unknown runner token".into()))?;
    Ok(format!("{}#{}", r.name, r.id))
}

pub async fn runner_protocol(c: &Ctx, t: &[&str]) -> ApiResult {
    let runner = runner_name(c).await?;
    match t {
        ["claim"] if c.method == Method::POST => {
            #[derive(Deserialize, Default)]
            struct B {
                #[serde(default)]
                labels: Vec<String>,
                #[serde(default)]
                wait: Option<u64>,
            }
            let b: B = c.parse().unwrap_or_default();
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(b.wait.unwrap_or(25).min(55));
            loop {
                // Register interest before checking, so a job queued in
                // between wakes us.
                let mut woken = std::pin::pin!(c.flux.ci_wake.notified());
                woken.as_mut().enable();
                if let Some(job) = c.flux.claim_job(&runner, &b.labels).await? {
                    return reply(job);
                }
                if tokio::time::timeout_at(deadline, woken).await.is_err() {
                    return no_content();
                }
            }
        }
        ["jobs", id, "log"] if c.method == Method::POST => {
            let seq = c.num("seq", 0)?;
            let cancelled = c.flux.job_log(&runner, num(id)?, seq, &c.body).await?;
            reply(json!({ "cancelled": cancelled }))
        }
        ["jobs", id, "finish"] if c.method == Method::POST => {
            #[derive(Deserialize)]
            struct B {
                conclusion: String,
                #[serde(default)]
                exit_code: Option<i64>,
            }
            let b: B = c.parse()?;
            c.flux.finish_job(&runner, num(id)?, &b.conclusion, b.exit_code).await?;
            no_content()
        }
        _ => Err(not_found("no such endpoint")),
    }
}

/// `GET/POST /repos/{repo}/runs`, `GET runs/{id}`, `POST runs/{id}/cancel|rerun`.
pub async fn runs(c: &Ctx, repo: RepoId, t: &[&str]) -> ApiResult {
    match (t, &c.method) {
        ([], &Method::GET) => {
            c.flux.authorize(&c.who, repo, Role::Read).await?;
            let cs = match c.q("changeset").or(c.q("rev")) {
                Some(r) => Some(c.flux.resolve_rev(repo, r).await?),
                None => None,
            };
            let (limit, _) = c.page(30, 200)?;
            let runs = c.flux.meta.list_ci_runs(repo, cs.as_ref(), limit).await?;
            reply(runs.iter().map(run_json).collect::<Vec<_>>())
        }
        ([], &Method::POST) => {
            #[derive(Deserialize)]
            struct B {
                workflow: String,
                #[serde(default, rename = "ref")]
                rev: Option<String>,
            }
            let b: B = c.parse()?;
            let rev = match b.rev {
                Some(r) => r,
                None => super::code::default_branch(&c.flux, repo).await?.unwrap_or_else(|| "main".into()),
            };
            let ids = c.flux.dispatch_workflow(&c.who, repo, &b.workflow, &rev).await?;
            if ids.is_empty() {
                return Err(bad(format!("{} has no workflow_dispatch trigger (or does not exist)", b.workflow)));
            }
            created(json!({ "runs": ids }))
        }
        ([id], &Method::GET) => {
            c.flux.authorize(&c.who, repo, Role::Read).await?;
            let run = c.flux.meta.get_ci_run(num(id)?).await?.filter(|r| r.repo_id == repo).ok_or_else(|| not_found("run"))?;
            let jobs = c.flux.meta.ci_jobs(run.id).await?;
            let mut v = run_json(&run);
            v["jobs"] = json!(
                jobs.iter()
                    .map(|j| {
                        let name = serde_json::from_str::<crate::ci::JobSpec>(&j.spec).map(|s| s.name).unwrap_or_else(|_| j.name.clone());
                        json!({ "id": j.id, "name": name, "state": j.state, "runner": j.runner, "started_at": j.started_ms, "finished_at": j.finished_ms, "exit_code": j.exit_code, "needs": j.needs, "labels": j.labels })
                    })
                    .collect::<Vec<_>>()
            );
            reply(v)
        }
        ([id, "cancel"], &Method::POST) => {
            c.flux.cancel_run(&c.who, repo, num(id)?).await?;
            no_content()
        }
        ([id, "rerun"], &Method::POST) => {
            let ids = c.flux.rerun(&c.who, repo, num(id)?).await?;
            created(json!({ "runs": ids }))
        }
        _ => Err(not_found("no such endpoint")),
    }
}

fn run_json(r: &flux_store::CiRun) -> serde_json::Value {
    json!({
        "id": r.id, "workflow": r.workflow, "event": r.event, "ref": r.ref_name, "changeset": r.cs.to_hex(),
        "review": r.review, "state": r.state, "actor": r.actor, "created_at": r.created_ms, "updated_at": r.updated_ms,
    })
}

/// A job's log (`?after=<seq>` for incremental reads; `seq` in the
/// `x-flux-log-seq` header).
pub async fn job_log(c: &Ctx, repo: RepoId, id: &str) -> ApiResult {
    c.flux.authorize(&c.who, repo, Role::Read).await?;
    let job = c.flux.meta.get_ci_job(num(id)?).await?.filter(|j| j.repo_id == repo).ok_or_else(|| not_found("job"))?;
    let chunks = c.flux.meta.ci_log(job.id, c.num("after", 0)?).await?;
    let last = chunks.last().map(|(s, _)| *s).unwrap_or(c.num("after", 0)?);
    let mut body = Vec::new();
    for (_, d) in chunks {
        body.extend_from_slice(&d);
    }
    let mut r = body.into_response();
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, axum::http::HeaderValue::from_static("text/plain; charset=utf-8"));
    if let Ok(v) = axum::http::HeaderValue::from_str(&last.to_string()) {
        h.insert("x-flux-log-seq", v);
    }
    if let Ok(v) = axum::http::HeaderValue::from_str(&job.state) {
        h.insert("x-flux-job-state", v);
    }
    Ok(r)
}
