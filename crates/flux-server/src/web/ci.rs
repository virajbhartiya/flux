//! Built-in CI: workflow runs, jobs with live logs, cancel, re-run and
//! manual dispatch.

use std::collections::HashMap;

use axum::http::{HeaderValue, header};
use axum::response::IntoResponse;
use flux_store::{CiJob, CiRun};
use maud::{Markup, html};

use super::forms::{Fields, done, to};
use super::pages::{RepoCtx, nav, parse_num};
use super::view::{self, Tab, avatar, empty_state, enc, icon, page, short, time, url};
use super::{Req, WebError, WebResult, WebState};
use crate::rbac::Role;

/// Initial log shown per job (the tail); the rest is a download away.
const MAX_LOG: usize = 1 << 20;

fn base(ctx: &RepoCtx) -> String {
    format!("/{}/-", enc(&ctx.name))
}

pub fn terminal(state: &str) -> bool {
    matches!(state, "success" | "failure" | "cancelled" | "skipped")
}

/// A state as an icon in a tinted circle.
pub fn state_icon(state: &str) -> Markup {
    let (cls, ic) = match state {
        "success" => ("success", "check"),
        "failure" | "error" => ("failure", "x"),
        "cancelled" => ("cancelled", "stop"),
        "skipped" => ("cancelled", "x"),
        "running" | "in_progress" => ("running", "refresh"),
        _ => ("pending", "clock"),
    };
    html! { span class=(format!("run-state {cls}")) title=(state) role="img" aria-label=(state) { (icon(ic)) } }
}

pub fn fmt_duration(ms: i64) -> String {
    let s = (ms / 1000).max(0);
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m {:02}s", s / 60, s % 60),
        _ => format!("{}h {:02}m", s / 3600, (s / 60) % 60),
    }
}

fn run_duration(r: &CiRun) -> String {
    if terminal(&r.state) { fmt_duration(r.updated_ms - r.created_ms) } else { fmt_duration(view::now_ms() - r.created_ms) }
}

fn event_label(r: &CiRun) -> String {
    match (r.event.as_str(), r.review) {
        ("pull_request", Some(n)) => format!("review #{n}"),
        ("workflow_dispatch", _) => "manual".into(),
        (e, _) => e.replace('_', " "),
    }
}

/// Drop ANSI escape sequences (colors, cursor moves) from log text.
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\u{1b}' {
            if it.peek() == Some(&'[') {
                it.next();
                for d in it.by_ref() {
                    if d.is_ascii_alphabetic() {
                        break;
                    }
                }
            } else {
                it.next();
            }
            continue;
        }
        if c != '\r' {
            out.push(c);
        }
    }
    out
}

fn job_name(j: &CiJob) -> String {
    serde_json::from_str::<crate::ci::JobSpec>(&j.spec).map(|s| s.name).unwrap_or_else(|_| j.name.clone())
}

/// Workflows at `rev` that can be started by hand: (file, name).
async fn dispatchable(web: &WebState, ctx: &RepoCtx, rev: &str, actor: &str) -> Vec<(String, String)> {
    let Ok(cs) = web.flux.resolve_rev(ctx.id, rev).await else { return vec![] };
    let Ok(files) = web.flux.workflow_files(ctx.id, &cs).await else { return vec![] };
    let t = crate::ci::Trigger {
        event: "workflow_dispatch".into(),
        ref_name: rev.into(),
        cs,
        before: None,
        review: None,
        actor: actor.into(),
        only: None,
    };
    files
        .iter()
        .filter_map(|(f, text)| match crate::ci::plan(f, text, &t, &[]) {
            Ok(Some((name, _))) => Some((f.clone(), name)),
            _ => None,
        })
        .collect()
}

pub async fn runs_get(web: &WebState, req: &Req, ctx: &RepoCtx, args: &[String], q: &HashMap<String, String>) -> WebResult {
    match args {
        [] => runs(web, req, ctx, q).await,
        [id] => run(web, req, ctx, parse_num(id)?).await,
        [id, j, job, l] if j == "jobs" && l == "log" => log(web, ctx, parse_num(id)?, parse_num(job)?, q).await,
        _ => Err(WebError::NotFound("no such page".into())),
    }
}

async fn runs(web: &WebState, req: &Req, ctx: &RepoCtx, q: &HashMap<String, String>) -> WebResult {
    let flux = &web.flux;
    let filter = q.get("workflow").filter(|w| !w.is_empty()).cloned();
    let mut list = flux.meta.list_ci_runs(ctx.id, None, 100).await?;
    let workflows: Vec<String> = {
        let mut w: Vec<String> = list.iter().map(|r| r.workflow.clone()).collect();
        w.sort();
        w.dedup();
        w
    };
    if let Some(w) = &filter {
        list.retain(|r| &r.workflow == w);
    }
    let rev = ctx.default_branch().unwrap_or("main").to_string();
    let can_write = ctx.can(Role::Write);
    let manual = if can_write { dispatchable(web, ctx, &rev, req.principal.name()).await } else { vec![] };
    let b = base(ctx);
    let body = html! {
        div.container {
            div.page-head.row {
                div { h1 { "CI" } p.muted { "Workflow runs from " code { ".flux-ci/workflows" } " or " code { ".github/workflows" } ". Results report as status checks." } }
                div.spacer {}
                @if !manual.is_empty() {
                    details.clone.dispatch {
                        summary.btn.primary { (icon("play")) "Run workflow" span.caret {} }
                        div.clone-menu {
                            form.form-tight method="post" action=(format!("{b}/runs/dispatch")) {
                                label { "Workflow"
                                    select name="workflow" { @for (f, name) in &manual { option value=(f) { (name) " · " (f) } } }
                                }
                                label { "Branch or changeset"
                                    input type="text" name="ref" value=(rev) list="ci-refs" required;
                                    datalist #ci-refs { @for (n, _) in ctx.refs.iter().take(200) { option value=(n) {} } }
                                }
                                div.form-actions { button.btn.primary type="submit" { (icon("play")) "Run" } }
                            }
                        }
                    }
                }
            }
            div.card {
                div.filter-tabs {
                    a.active[filter.is_none()] href=(format!("{b}/runs")) { "All workflows" }
                    @for w in &workflows {
                        a.active[filter.as_deref() == Some(w.as_str())] href=(format!("{b}/runs?workflow={}", enc(w))) { (w) }
                    }
                }
                @if list.is_empty() {
                    (empty_state("No runs yet", html! {
                        p { "Add a workflow file and push. Jobs run on " code { "flux-runner" } " machines registered with this server." }
                        pre.cmd { "mkdir -p .flux-ci/workflows\ncat > .flux-ci/workflows/ci.yml <<'EOF'\nname: CI\non: [push, pull_request]\njobs:\n  test:\n    runs-on: linux\n    steps:\n      - run: make test\nEOF" }
                    }))
                }
                ul.runs {
                    @for r in &list {
                        li.run-row {
                            (state_icon(&r.state))
                            div.run-main {
                                a.run-title href=(format!("{b}/runs/{}", r.id)) { (r.workflow) span.muted { " #" (r.id) } }
                                div.muted.small.run-meta {
                                    span.pill.small { (event_label(r)) }
                                    @if !r.ref_name.is_empty() { a.branch href=(url(&ctx.name, "tree", &r.ref_name, None)) { (icon("branch")) (r.ref_name) } }
                                    a.sha href=(url(&ctx.name, "commit", &r.cs.to_hex(), None)) { (short(&r.cs)) }
                                    span { "by " (r.actor) }
                                }
                            }
                            div.run-when.muted.small {
                                div { (time(r.created_ms)) }
                                div title="Duration" { (icon("clock")) " " (run_duration(r)) }
                            }
                        }
                    }
                }
            }
        }
    };
    let n = nav(web, req, ctx, Tab::Ci, &rev).await;
    Ok(page(web, req, &format!("CI · {}", ctx.name), Some(n), body))
}

async fn run(web: &WebState, req: &Req, ctx: &RepoCtx, id: i64) -> WebResult {
    let flux = &web.flux;
    let r = flux
        .meta
        .get_ci_run(id)
        .await?
        .filter(|r| r.repo_id == ctx.id)
        .ok_or_else(|| WebError::NotFound(format!("no run #{id}")))?;
    let jobs = flux.meta.ci_jobs(r.id).await?;
    let mut logs = Vec::with_capacity(jobs.len());
    for j in &jobs {
        let chunks = flux.meta.ci_log(j.id, 0).await?;
        let seq = chunks.last().map(|c| c.0).unwrap_or(0);
        let mut data: Vec<u8> = chunks.into_iter().flat_map(|c| c.1).collect();
        let cut = data.len() > MAX_LOG;
        if cut {
            data.drain(..data.len() - MAX_LOG);
        }
        logs.push((strip_ansi(&String::from_utf8_lossy(&data)), seq, cut));
    }
    let can_write = ctx.can(Role::Write);
    let b = base(ctx);
    let running = !terminal(&r.state);
    let body = html! {
        div.container.wide {
            div.review-head {
                div.title-row {
                    (state_icon(&r.state))
                    h1 { (r.workflow) span.muted { " #" (r.id) } }
                    div.spacer {}
                    @if can_write {
                        @if running {
                            form method="post" action=(format!("{b}/runs/{}/cancel", r.id)) data-confirm="Cancel this run?" {
                                button.btn.small.danger type="submit" { (icon("stop")) "Cancel run" }
                            }
                        }
                        form method="post" action=(format!("{b}/runs/{}/rerun", r.id)) {
                            button.btn.small type="submit" { (icon("refresh")) "Re-run" }
                        }
                    }
                }
                div.review-sub {
                    span class=(format!("state ci-{}", r.state)) { (r.state) }
                    span.pill.small { (event_label(&r)) }
                    @if let Some(n) = r.review { a.link href=(format!("{b}/reviews/{n}")) { "Review #" (n) } }
                    @if !r.ref_name.is_empty() { a.branch href=(url(&ctx.name, "tree", &r.ref_name, None)) { (icon("branch")) (r.ref_name) } }
                    a.sha href=(url(&ctx.name, "commit", &r.cs.to_hex(), None)) { (short(&r.cs)) }
                    span.muted { "triggered by " } (avatar(&r.actor, "", false)) strong { (r.actor) }
                    span.muted { (time(r.created_ms)) " · " (icon("clock")) " " (run_duration(&r)) }
                }
            }
            div.run-grid {
                nav.job-nav aria-label="Jobs" {
                    div.side-title { "Jobs" }
                    @for j in &jobs {
                        a.side-link href=(format!("#job-{}", j.id)) { (state_icon(&j.state)) span { (job_name(j)) } }
                    }
                    @if jobs.is_empty() { p.muted.small { "No jobs." } }
                }
                div {
                    @for (j, (text, seq, cut)) in jobs.iter().zip(&logs) {
                        div.card.job id=(format!("job-{}", j.id)) {
                            div.card-head.job-head {
                                (state_icon(&j.state))
                                strong { (job_name(j)) }
                                span.muted.small.job-state { (j.state) }
                                div.spacer {}
                                @if !j.runner.is_empty() { span.muted.small title="Runner" { (icon("monitor")) " " (j.runner) } }
                                @match (j.started_ms, j.finished_ms) {
                                    (Some(s), Some(f)) => { span.muted.small { (icon("clock")) " " (fmt_duration(f - s)) } }
                                    (Some(s), None) => { span.muted.small { "started " (time(s)) } }
                                    _ => {}
                                }
                                @if let Some(c) = j.exit_code { @if c != 0 { span.pill.small.danger { "exit " (c) } } }
                            }
                            @if !j.needs.is_empty() { div.notice.small { "Needs: " (j.needs.join(", ")) } }
                            @if *cut { div.notice.small { "Showing the last 1 MB of this log." } }
                            pre.ci-log data-log-src=(format!("{b}/runs/{}/jobs/{}/log", r.id, j.id)) data-seq=(seq) data-state=(j.state) tabindex="0" aria-label=(format!("Log for {}", job_name(j))) {
                                @if text.is_empty() {
                                    span.log-empty { @if terminal(&j.state) { "No output." } @else { "Waiting for a runner…" } }
                                }
                                (text)
                            }
                        }
                    }
                }
            }
        }
    };
    let rev = ctx.default_branch().unwrap_or("main").to_string();
    let n = nav(web, req, ctx, Tab::Ci, &rev).await;
    Ok(page(web, req, &format!("{} #{} · {}", r.workflow, r.id, ctx.name), Some(n), body))
}

/// Incremental log text: `?after=<seq>`; the last sequence number and the
/// job state come back in headers.
async fn log(web: &WebState, ctx: &RepoCtx, run: i64, job: i64, q: &HashMap<String, String>) -> WebResult {
    let flux = &web.flux;
    let j = flux
        .meta
        .get_ci_job(job)
        .await?
        .filter(|j| j.repo_id == ctx.id && j.run_id == run)
        .ok_or_else(|| WebError::NotFound("no such job".into()))?;
    let after: i64 = q.get("after").and_then(|a| a.parse().ok()).unwrap_or(0);
    let chunks = flux.meta.ci_log(j.id, after).await?;
    let last = chunks.last().map(|c| c.0).unwrap_or(after);
    let data: Vec<u8> = chunks.into_iter().flat_map(|c| c.1).collect();
    let mut r = strip_ansi(&String::from_utf8_lossy(&data)).into_response();
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=utf-8"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    if let Ok(v) = HeaderValue::from_str(&last.to_string()) {
        h.insert("x-flux-log-seq", v);
    }
    if let Ok(v) = HeaderValue::from_str(&j.state) {
        h.insert("x-flux-job-state", v);
    }
    Ok(r)
}

pub async fn post(web: &WebState, req: &Req, ctx: &RepoCtx, s: &[&str], f: &Fields) -> WebResult {
    let flux = &web.flux;
    let who = &req.principal;
    let b = base(ctx);
    match s {
        ["runs", "dispatch"] => {
            let rev = if f.trim("ref").is_empty() { ctx.default_branch().unwrap_or("main") } else { f.trim("ref") };
            match flux.dispatch_workflow(who, ctx.id, f.trim("workflow"), rev).await {
                Ok(ids) if !ids.is_empty() => Ok(to(&format!("{b}/runs/{}", ids[0]))),
                Ok(_) => done(
                    &format!("{b}/runs"),
                    Err::<String, _>(WebError::BadRequest("that workflow has no workflow_dispatch trigger".into())),
                ),
                Err(e) => done(&format!("{b}/runs"), Err::<String, _>(e)),
            }
        }
        ["runs", id, "cancel"] => {
            let id = parse_num(id)?;
            done(&format!("{b}/runs/{id}"), flux.cancel_run(who, ctx.id, id).await.map(|_| "Run cancelled".to_string()))
        }
        ["runs", id, "rerun"] => {
            let id = parse_num(id)?;
            match flux.rerun(who, ctx.id, id).await {
                Ok(ids) if !ids.is_empty() => Ok(to(&format!("{b}/runs/{}", ids[0]))),
                Ok(_) => done(
                    &format!("{b}/runs/{id}"),
                    Err::<String, _>(WebError::Conflict("the workflow no longer matches this event".into())),
                ),
                Err(e) => done(&format!("{b}/runs/{id}"), Err::<String, _>(e)),
            }
        }
        _ => Err(WebError::NotFound("no such action".into())),
    }
}

/// Status dots for changesets, batched: id → (combined state, statuses).
pub async fn statuses_for(
    web: &WebState,
    ctx: &RepoCtx,
    ids: &[flux_core::ChangesetId],
) -> HashMap<flux_core::ChangesetId, Vec<flux_store::CommitStatus>> {
    let mut out: HashMap<flux_core::ChangesetId, Vec<flux_store::CommitStatus>> = HashMap::new();
    if ids.is_empty() {
        return out;
    }
    if let Ok(rows) = web.flux.meta.statuses_many(ctx.id, ids).await {
        for (id, s) in rows {
            out.entry(id).or_default().push(s);
        }
    }
    out
}

/// A combined status dot with a hover list of contexts.
pub fn status_badge(statuses: &[flux_store::CommitStatus], href: &str) -> Markup {
    let Some(state) = view::combine_states(statuses.iter().map(|s| s.state.as_str())) else { return html! {} };
    let ok = statuses.iter().filter(|s| s.state == "success").count();
    let title = statuses.iter().map(|s| format!("{}: {}", s.context, s.state)).collect::<Vec<_>>().join("\n");
    html! {
        a.status-badge href=(href) title=(format!("{ok}/{} checks passed\n{title}", statuses.len())) aria-label=(format!("Checks: {state}")) {
            (view::sdot(state))
        }
    }
}

/// Built-in CI links point at this server: keep them relative so they work
/// whatever host the page was reached through.
pub fn check_href(s: &flux_store::CommitStatus) -> String {
    if s.creator == "flux-ci"
        && let Some(rest) = s.target_url.split_once("://").map(|(_, r)| r)
        && let Some(i) = rest.find('/')
    {
        return rest[i..].to_string();
    }
    s.target_url.clone()
}

/// The list of checks for a changeset (commit and review pages).
pub fn checks_list(statuses: &[flux_store::CommitStatus]) -> Markup {
    html! {
        ul.checks {
            @for s in statuses {
                li {
                    (view::sdot(&s.state))
                    strong { (s.context) }
                    span.muted.small { (if s.description.is_empty() { s.state.as_str() } else { s.description.as_str() }) }
                    div.spacer {}
                    @if !s.target_url.is_empty() { a.link.small href=(check_href(s)) { "Details" } }
                }
            }
        }
    }
}
