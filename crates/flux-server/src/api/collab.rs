//! Collaboration endpoints: account, keys, tokens, notifications, issues,
//! labels, milestones, reviews, webhooks, protection, releases, forks and
//! secrets.

use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::IntoResponse;
use flux_store::{IssueQuery, Label, RepoId};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{ApiError, ApiResult, Ctx, bad, created, method_not_allowed, no_content, not_found, reply};
use crate::hooks::{HookInput, Scope};
use crate::issues::{IssueEdit, IssueInput};
use crate::rbac::Role;

fn num(s: &str) -> ApiResult<i64> {
    s.parse().map_err(|_| bad(format!("{s:?} is not a number")))
}

// ───────────── account ─────────────

pub async fn me(c: &Ctx) -> ApiResult {
    let u = c.user()?;
    let acct = c.flux.meta.get_account_by_id(u.id).await?;
    reply(json!({
        "login": u.name,
        "id": u.id,
        "name": acct.as_ref().map(|a| a.display_name.clone()),
        "email": acct.as_ref().and_then(|a| a.email.clone()),
        "site_admin": u.is_admin,
        "kind": c.flux.meta.user_kind(u.id).await?,
        "token_scopes": c.who.scopes,
        "unread_notifications": c.flux.meta.unread_count(u.id).await?,
        "two_factor": c.flux.meta.totp(u.id).await?.0.is_some(),
    }))
}

pub async fn list_keys(c: &Ctx) -> ApiResult {
    let u = c.user()?;
    let keys = c.flux.meta.list_ssh_keys(u.id).await?;
    reply(
        keys.iter()
            .map(|k| json!({ "id": k.id, "title": k.title, "fingerprint": k.fingerprint, "key": k.public_key, "created_at": k.created_ms, "last_used": k.last_used }))
            .collect::<Vec<_>>(),
    )
}

#[derive(Deserialize)]
struct KeyIn {
    #[serde(default)]
    title: String,
    key: String,
}

pub async fn add_key(c: &Ctx) -> ApiResult {
    let k: KeyIn = c.parse()?;
    let saved = c.flux.add_ssh_key(&c.who, &k.title, &k.key).await?;
    created(json!({ "id": saved.id, "title": saved.title, "fingerprint": saved.fingerprint }))
}

pub async fn delete_key(c: &Ctx, id: &str) -> ApiResult {
    c.flux.delete_ssh_key(&c.who, num(id)?).await?;
    no_content()
}

#[derive(Deserialize)]
struct TokenIn {
    #[serde(default)]
    label: String,
    /// `read`, `write` or `all`.
    #[serde(default = "read")]
    scope: String,
    /// Restrict to these repositories.
    #[serde(default)]
    repos: Vec<String>,
    /// Restrict ref updates to these patterns (`agent/*`).
    #[serde(default)]
    refs: Vec<String>,
    #[serde(default)]
    expires_in_days: Option<i64>,
}

fn read() -> String {
    "read".into()
}

/// Mint a token, optionally bound to repositories and ref patterns — the
/// way to hand an agent exactly the access it needs.
pub async fn create_token(c: &Ctx) -> ApiResult {
    let t: TokenIn = c.parse()?;
    let mut scopes = t.scope.clone();
    for r in &t.repos {
        let id = c.flux.repo(r).await?;
        c.flux.authorize(&c.who, id, Role::Read).await?;
        scopes.push_str(&format!(" repo:{}", id.0));
    }
    for r in &t.refs {
        scopes.push_str(&format!(" ref:{r}"));
    }
    let token = c.flux.create_api_token(&c.who, &t.label, &scopes, t.expires_in_days).await?;
    created(json!({ "token": token, "scopes": scopes }))
}

pub async fn notifications(c: &Ctx) -> ApiResult {
    let u = c.user()?;
    let (limit, _) = c.page(50, 200)?;
    let list = c.flux.meta.notifications(u.id, !c.flag("all"), limit).await?;
    let base = c.base_url();
    let mut out = Vec::new();
    for n in list {
        let repo = c.flux.repo_name(n.repo_id).await;
        let path = if n.kind == "issue" { "issues" } else { "reviews" };
        out.push(json!({
            "id": n.id, "repo": repo, "number": n.number, "kind": n.kind, "reason": n.reason, "title": n.title,
            "unread": n.unread, "updated_at": n.updated_ms, "url": format!("{base}/{repo}/-/{path}/{}", n.number),
        }));
    }
    reply(out)
}

pub async fn mark_read(c: &Ctx, id: Option<&str>) -> ApiResult {
    let u = c.user()?;
    let id = id.map(num).transpose()?;
    let n = c.flux.meta.mark_read(u.id, id).await?;
    reply(json!({ "marked": n }))
}

#[derive(Deserialize)]
struct SubIn {
    /// `subscribed`/`watching`, `ignored`, or `none`.
    state: String,
}

pub async fn subscribe(c: &Ctx, repo: RepoId, number: i64) -> ApiResult {
    c.flux.authorize(&c.who, repo, Role::Read).await?;
    let u = c.user()?;
    let s: SubIn = c.parse()?;
    let state = match s.state.as_str() {
        "subscribed" | "watching" => Some("subscribed"),
        "ignored" => Some("ignored"),
        "none" | "" => None,
        _ => return Err(bad("state: watching | ignored | none")),
    };
    c.flux.meta.set_subscription(u.id, repo, number, state).await?;
    reply(json!({ "state": state.unwrap_or("none") }))
}

// ───────────── issues ─────────────

pub async fn issues(c: &Ctx, repo: RepoId, t: &[&str]) -> ApiResult {
    let m = &c.method;
    match t {
        [] if m == Method::GET => {
            let (limit, offset) = c.page(30, 100)?;
            let q = IssueQuery {
                state: match c.q("state").unwrap_or("open") {
                    "all" => None,
                    s => Some(s.to_string()),
                },
                label: c.q("label").or(c.q("labels")).map(str::to_string),
                assignee: c.q("assignee").map(str::to_string),
                author: c.q("author").or(c.q("creator")).map(str::to_string),
                milestone: c.q("milestone").map(num).transpose()?,
                text: c.q("q").map(str::to_string),
                sort: c.q("sort").map(str::to_string),
                limit,
                offset,
            };
            let (items, total) = c.flux.list_issues_as(&c.who, repo, &q).await?;
            let mut r = axum::Json(items).into_response();
            if let Ok(v) = HeaderValue::from_str(&total.to_string()) {
                r.headers_mut().insert("x-total-count", v);
            }
            Ok(r)
        }
        [] if m == Method::POST => {
            let input: IssueInput = c.parse()?;
            created(c.flux.open_issue(&c.who, repo, input).await?)
        }
        ["comments", id] if m == Method::PATCH => {
            #[derive(Deserialize)]
            struct B {
                body: String,
            }
            let b: B = c.parse()?;
            c.flux.change_comment(&c.who, repo, num(id)?, Some(&b.body)).await?;
            no_content()
        }
        ["comments", id] if m == Method::DELETE => {
            c.flux.change_comment(&c.who, repo, num(id)?, None).await?;
            no_content()
        }
        [n] if m == Method::GET => reply(c.flux.issue_view(&c.who, repo, num(n)?).await?),
        [n] if m == Method::PATCH => {
            let e: IssueEdit = c.parse()?;
            reply(c.flux.edit_issue(&c.who, repo, num(n)?, e).await?)
        }
        [n, "comments"] if m == Method::GET => {
            let v = c.flux.issue_view(&c.who, repo, num(n)?).await?;
            reply(v.timeline.into_iter().filter(|e| e.kind == "comment").collect::<Vec<_>>())
        }
        [n, "comments"] if m == Method::POST => {
            #[derive(Deserialize)]
            struct B {
                body: String,
            }
            let b: B = c.parse()?;
            created(c.flux.comment_issue(&c.who, repo, num(n)?, &b.body).await?)
        }
        [n, "labels"] if m == Method::PUT || m == Method::POST => {
            #[derive(Deserialize)]
            struct B {
                labels: Vec<String>,
            }
            let b: B = c.parse()?;
            let role = c.flux.repo_role(&c.who, repo).await?;
            c.flux.apply_labels(&c.who, repo, num(n)?, &b.labels, role).await?;
            no_content()
        }
        [n, "assignees"] if m == Method::PUT || m == Method::POST => {
            #[derive(Deserialize)]
            struct B {
                assignees: Vec<String>,
            }
            let b: B = c.parse()?;
            c.flux.set_people(&c.who, repo, num(n)?, "assignee", &b.assignees).await?;
            no_content()
        }
        [n, "subscription"] if m == Method::PUT => subscribe(c, repo, num(n)?).await,
        _ => Err(not_found("no such endpoint")),
    }
}

pub async fn labels(c: &Ctx, repo: RepoId, t: &[&str]) -> ApiResult {
    #[derive(Deserialize)]
    struct L {
        #[serde(default)]
        name: String,
        #[serde(default)]
        color: Option<String>,
        #[serde(default)]
        description: Option<String>,
    }
    let m = &c.method;
    match t {
        [] if m == Method::GET => {
            c.flux.authorize(&c.who, repo, Role::Read).await?;
            let labels = c.flux.meta.list_labels(repo).await?;
            reply(
                labels
                    .iter()
                    .map(|l| json!({ "name": l.name, "color": l.color, "description": l.description }))
                    .collect::<Vec<_>>(),
            )
        }
        [] if m == Method::POST => {
            let l: L = c.parse()?;
            c.flux
                .put_label(
                    &c.who,
                    repo,
                    None,
                    Label {
                        name: l.name.clone(),
                        color: l.color.unwrap_or_else(|| "8a8f98".into()),
                        description: l.description.unwrap_or_default(),
                    },
                )
                .await?;
            created(json!({ "name": l.name }))
        }
        [name] if m == Method::PATCH => {
            let l: L = c.parse()?;
            let cur =
                c.flux.meta.list_labels(repo).await?.into_iter().find(|x| x.name == *name).ok_or_else(|| not_found("label"))?;
            let new = Label {
                name: if l.name.is_empty() { cur.name.clone() } else { l.name },
                color: l.color.unwrap_or(cur.color),
                description: l.description.unwrap_or(cur.description),
            };
            c.flux.put_label(&c.who, repo, Some(name), new.clone()).await?;
            reply(json!({ "name": new.name, "color": new.color, "description": new.description }))
        }
        [name] if m == Method::DELETE => {
            c.flux.delete_label(&c.who, repo, name).await?;
            no_content()
        }
        _ => Err(not_found("no such endpoint")),
    }
}

pub async fn milestones(c: &Ctx, repo: RepoId, t: &[&str]) -> ApiResult {
    #[derive(Deserialize)]
    struct M {
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        description: Option<String>,
        #[serde(default)]
        state: Option<String>,
        #[serde(default)]
        due_on: Option<i64>,
    }
    let m = &c.method;
    let list = |ms: Vec<flux_store::Milestone>| {
        ms.iter()
            .map(|x| json!({ "number": x.number, "title": x.title, "description": x.description, "state": x.state, "due_on": x.due_ms,
                "open_issues": x.open_items, "closed_issues": x.closed_items, "created_at": x.created_ms, "closed_at": x.closed_ms }))
            .collect::<Vec<_>>()
    };
    match t {
        [] if m == Method::GET => {
            c.flux.authorize(&c.who, repo, Role::Read).await?;
            reply(list(c.flux.meta.list_milestones(repo).await?))
        }
        [] if m == Method::POST => {
            let b: M = c.parse()?;
            let ms = c
                .flux
                .create_milestone(
                    &c.who,
                    repo,
                    b.title.as_deref().unwrap_or(""),
                    b.description.as_deref().unwrap_or(""),
                    b.due_on,
                )
                .await?;
            created(list(vec![ms]).remove(0))
        }
        [n] if m == Method::PATCH => {
            let b: M = c.parse()?;
            let n = num(n)?;
            let cur = c
                .flux
                .meta
                .list_milestones(repo)
                .await?
                .into_iter()
                .find(|x| x.number == n)
                .ok_or_else(|| not_found("milestone"))?;
            c.flux
                .update_milestone(
                    &c.who,
                    repo,
                    n,
                    b.title.as_deref().unwrap_or(&cur.title),
                    b.description.as_deref().unwrap_or(&cur.description),
                    b.state.as_deref().unwrap_or(&cur.state),
                    b.due_on.or(cur.due_ms),
                )
                .await?;
            no_content()
        }
        [n] if m == Method::DELETE => {
            c.flux.delete_milestone(&c.who, repo, num(n)?).await?;
            no_content()
        }
        _ => Err(not_found("no such endpoint")),
    }
}

// ───────────── reviews ─────────────

async fn review_summary(c: &Ctx, repo: RepoId, r: &flux_store::Review) -> ApiResult<Value> {
    let (draft, source) = c.flux.meta.review_flags(repo, r.number).await?;
    let name = c.flux.repo_name(repo).await;
    Ok(json!({
        "number": r.number,
        "title": r.title,
        "body": r.body,
        "author": r.author,
        "state": r.state,
        "draft": draft,
        "head": r.head.to_hex(),
        "target": String::from_utf8_lossy(&r.target),
        "landed": r.landed.map(|l| l.to_hex()),
        "source_repo": if source > 0 { Some(c.flux.repo_name(RepoId(source)).await) } else { None },
        "created_at": r.created_ms,
        "updated_at": r.updated_ms,
        "html_url": format!("{}/{name}/-/reviews/{}", c.base_url(), r.number),
    }))
}

#[derive(Deserialize)]
struct NewReview {
    title: String,
    #[serde(default)]
    body: String,
    head: String,
    #[serde(default)]
    target: Option<String>,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    reviewers: Vec<String>,
    #[serde(default)]
    labels: Vec<String>,
    /// Open from a fork: the fork's repository name.
    #[serde(default)]
    source: Option<String>,
}

#[derive(Deserialize, Default)]
struct ReviewPatch {
    title: Option<String>,
    body: Option<String>,
    /// `open` or `closed`.
    state: Option<String>,
    /// A new head revision (the stack was updated).
    head: Option<String>,
    draft: Option<bool>,
    labels: Option<Vec<String>>,
    reviewers: Option<Vec<String>>,
    assignees: Option<Vec<String>>,
}

pub async fn reviews(c: &Ctx, repo: RepoId, t: &[&str]) -> ApiResult {
    let m = &c.method;
    match t {
        [] if m == Method::GET => {
            c.flux.authorize(&c.who, repo, Role::Read).await?;
            let state = match c.q("state").unwrap_or("open") {
                "all" => None,
                s => Some(s),
            };
            let (limit, _) = c.page(30, 500)?;
            let list = c.flux.meta.list_reviews(repo, state, limit).await?;
            let mut out = Vec::new();
            for r in &list {
                if c.q("author").is_some_and(|a| a != r.author) {
                    continue;
                }
                out.push(review_summary(c, repo, r).await?);
            }
            reply(out)
        }
        [] if m == Method::POST => {
            let r: NewReview = c.parse()?;
            c.flux.authorize(&c.who, repo, Role::Read).await?;
            c.flux.check_writable(repo).await?;
            let target = match r.target {
                Some(t) => t,
                None => super::code::default_branch(&c.flux, repo).await?.unwrap_or_else(|| "main".into()),
            };
            let review = match r.source {
                Some(src) => {
                    let fork = c.flux.repo(&src).await?;
                    let head = c.flux.resolve_rev(fork, &r.head).await?;
                    c.flux.review_from_fork(&c.who, repo, fork, head, target.as_bytes(), &r.title, &r.body).await?
                }
                None => {
                    // Opening a review on the repository itself needs write.
                    c.flux.authorize(&c.who, repo, Role::Write).await?;
                    let head = c.flux.resolve_rev(repo, &r.head).await?;
                    c.flux.create_review(repo, &r.title, &r.body, c.who.name(), head, target.as_bytes()).await?
                }
            };
            if r.draft {
                c.flux.meta.set_review_draft(repo, review.number, true).await?;
            }
            let role = c.flux.repo_role(&c.who, repo).await?;
            if !r.labels.is_empty() && role >= Role::Triage {
                c.flux.apply_labels(&c.who, repo, review.number, &r.labels, role).await?;
            }
            if !r.reviewers.is_empty() && role >= Role::Triage {
                c.flux.set_people(&c.who, repo, review.number, "reviewer", &r.reviewers).await?;
            }
            if let Some(u) = &c.who.user {
                c.flux.subscribe_participant(u.id, repo, review.number).await;
            }
            created(review_summary(c, repo, &review).await?)
        }
        [n] if m == Method::GET => review_detail(c, repo, num(n)?).await,
        [n] if m == Method::PATCH => {
            let n = num(n)?;
            let p: ReviewPatch = c.parse()?;
            let rv = c.flux.review(repo, n).await?;
            let is_author = c.who.user.as_ref().is_some_and(|u| u.name == rv.author);
            let role = c.flux.repo_role(&c.who, repo).await?;
            if (p.title.is_some() || p.body.is_some()) && !(is_author || role >= Role::Triage) {
                return Err(ApiError(StatusCode::FORBIDDEN, "only the author or triagers can edit this review".into()));
            }
            if p.title.is_some() || p.body.is_some() {
                c.flux
                    .meta
                    .update_review(
                        repo,
                        n,
                        flux_store::ReviewUpdate { title: p.title.clone(), body: p.body.clone(), ..Default::default() },
                    )
                    .await?;
            }
            if let Some(h) = &p.head {
                c.flux
                    .authorize(
                        &c.who,
                        repo,
                        crate::rbac::review_action_role("update", is_author).max(if is_author {
                            Role::Read
                        } else {
                            Role::Write
                        }),
                    )
                    .await?;
                let head = c.flux.resolve_rev(repo, h).await?;
                c.flux.update_review(repo, n, "update", c.who.name(), "", Some(head)).await?;
            }
            if let Some(s) = &p.state {
                let action = match s.as_str() {
                    "closed" => "close",
                    "open" => "reopen",
                    _ => return Err(bad("state: open | closed")),
                };
                c.flux.authorize(&c.who, repo, crate::rbac::review_action_role(action, is_author)).await?;
                c.flux.update_review(repo, n, action, c.who.name(), "", None).await?;
            }
            if let Some(d) = p.draft {
                if !(is_author || role >= Role::Write) {
                    return Err(ApiError(StatusCode::FORBIDDEN, "only the author can change draft state".into()));
                }
                c.flux.meta.set_review_draft(repo, n, d).await?;
                if !d {
                    c.flux.review_event(repo, n, "ready", c.who.name()).await;
                }
            }
            if let Some(l) = &p.labels {
                c.flux.apply_labels(&c.who, repo, n, l, role).await?;
            }
            if let Some(r) = &p.reviewers {
                c.flux.set_people(&c.who, repo, n, "reviewer", r).await?;
            }
            if let Some(a) = &p.assignees {
                c.flux.set_people(&c.who, repo, n, "assignee", a).await?;
            }
            review_detail(c, repo, n).await
        }
        [n, "comments"] if m == Method::POST => {
            #[derive(Deserialize)]
            struct B {
                body: String,
                #[serde(default)]
                path: Option<String>,
                #[serde(default)]
                line: Option<i64>,
            }
            let b: B = c.parse()?;
            let n = num(n)?;
            c.flux.authorize(&c.who, repo, Role::Read).await?;
            c.user()?;
            match (b.path, b.line) {
                (Some(p), Some(l)) => c.flux.comment_review_at(repo, n, c.who.name(), &b.body, p.as_bytes(), l).await?,
                _ => {
                    c.flux.update_review(repo, n, "comment", c.who.name(), &b.body, None).await?;
                }
            }
            if let Some(u) = &c.who.user {
                c.flux.subscribe_participant(u.id, repo, n).await;
            }
            created(json!({ "ok": true }))
        }
        [n, action @ ("approve" | "request-changes" | "request_changes" | "dismiss")] if m == Method::POST => {
            #[derive(Deserialize, Default)]
            struct B {
                #[serde(default)]
                body: String,
            }
            let b: B = c.parse().unwrap_or_default();
            let n = num(n)?;
            c.flux.authorize(&c.who, repo, Role::Read).await?;
            c.user()?;
            let rv = c.flux.review(repo, n).await?;
            if c.who.user.as_ref().is_some_and(|u| u.name == rv.author) && *action == "approve" {
                return Err(bad("authors cannot approve their own review"));
            }
            let act = match *action {
                "approve" => "approve",
                "dismiss" => "dismiss",
                _ => "request_changes",
            };
            c.flux.update_review(repo, n, act, c.who.name(), &b.body, None).await?;
            reply(c.flux.review_gate(repo, n).await?)
        }
        [n, "land"] | [n, "merge"] if m == Method::POST || m == Method::PUT => {
            #[derive(Deserialize, Default)]
            struct B {
                #[serde(default)]
                method: String,
            }
            let b: B = c.parse().unwrap_or_default();
            let method: crate::protect::MergeMethod = b.method.parse()?;
            let r = c.flux.land_review_as(&c.who, repo, num(n)?, method).await?;
            reply(
                json!({ "landed": true, "head": r.head.to_hex(), "fast_forward": r.fast_forward, "rebased": r.rebased.len(), "merged_files": r.merged_files }),
            )
        }
        [n, "gate"] => {
            c.flux.authorize(&c.who, repo, Role::Read).await?;
            reply(c.flux.review_gate(repo, num(n)?).await?)
        }
        [n, "diff"] => {
            c.flux.authorize(&c.who, repo, Role::Read).await?;
            let d = c.flux.review_detail(repo, num(n)?).await?;
            let base = match d.target_head {
                Some(t) => c.flux.merge_base(repo, &t, &d.review.head).await?,
                None => None,
            };
            let patch = super::code::unified_diff(&c.flux, repo, &c.who, base, Some(d.review.head), None).await?;
            let mut r = patch.into_response();
            r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/x-diff; charset=utf-8"));
            Ok(r)
        }
        [n, "reviewers"] if m == Method::PUT || m == Method::POST => {
            #[derive(Deserialize)]
            struct B {
                reviewers: Vec<String>,
            }
            let b: B = c.parse()?;
            c.flux.set_people(&c.who, repo, num(n)?, "reviewer", &b.reviewers).await?;
            no_content()
        }
        [n, "labels"] if m == Method::PUT || m == Method::POST => {
            #[derive(Deserialize)]
            struct B {
                labels: Vec<String>,
            }
            let b: B = c.parse()?;
            let role = c.flux.repo_role(&c.who, repo).await?;
            c.flux.apply_labels(&c.who, repo, num(n)?, &b.labels, role).await?;
            no_content()
        }
        [n, "subscription"] if m == Method::PUT => subscribe(c, repo, num(n)?).await,
        _ => Err(not_found("no such endpoint")),
    }
}

async fn review_detail(c: &Ctx, repo: RepoId, n: i64) -> ApiResult {
    c.flux.authorize(&c.who, repo, Role::Read).await?;
    let d = c.flux.review_detail(repo, n).await?;
    let mut v = review_summary(c, repo, &d.review).await?;
    let mut stack = Vec::new();
    for l in &d.stack {
        stack.push(super::code::commit_json(&c.flux, repo, l, false, true).await);
    }
    v["stack"] = json!(stack);
    v["target_head"] = json!(d.target_head.map(|t| t.to_hex()));
    v["events"] = json!(
        d.events
            .iter()
            .map(|e| json!({ "id": e.seq, "kind": e.kind, "author": e.author, "body": e.body, "changeset": e.cs.map(|c| c.to_hex()),
                "path": e.path.as_ref().map(|p| String::from_utf8_lossy(p).to_string()), "line": e.line, "created_at": e.ts_ms }))
            .collect::<Vec<_>>()
    );
    v["labels"] = json!(c.flux.meta.item_labels(repo, &[n]).await?.into_iter().map(|(_, l)| l).collect::<Vec<_>>());
    v["reviewers"] =
        json!(c.flux.meta.item_people(repo, &[n], "reviewer").await?.into_iter().map(|(_, p)| p).collect::<Vec<_>>());
    v["assignees"] =
        json!(c.flux.meta.item_people(repo, &[n], "assignee").await?.into_iter().map(|(_, p)| p).collect::<Vec<_>>());
    v["gate"] = json!(c.flux.review_gate(repo, n).await?);
    v["checks"] = json!(c.flux.combined_status(repo, &d.review.head).await?);
    reply(v)
}

// ───────────── webhooks ─────────────

pub async fn hooks(c: &Ctx, scope: Scope, t: &[&str]) -> ApiResult {
    let m = &c.method;
    match t {
        [] if m == Method::GET => reply(c.flux.list_hooks(&c.who, scope).await?),
        [] if m == Method::POST => {
            let h: HookInput = c.parse()?;
            created(c.flux.create_hook(&c.who, scope, h).await?)
        }
        [id] if m == Method::GET => {
            let h = c.flux.hook_in(&c.who, scope, num(id)?).await?;
            reply(crate::hooks::HookJson::from(&h))
        }
        [id] if m == Method::PATCH => {
            let h: HookInput = c.parse()?;
            reply(c.flux.update_hook(&c.who, scope, num(id)?, h).await?)
        }
        [id] if m == Method::DELETE => {
            c.flux.delete_hook(&c.who, scope, num(id)?).await?;
            no_content()
        }
        [id, "deliveries"] => reply(c.flux.hook_deliveries(&c.who, scope, num(id)?).await?),
        [id, "deliveries", d, "redeliver"] | [id, "deliveries", d, "attempts"] if m == Method::POST => {
            let n = c.flux.redeliver(&c.who, scope, num(id)?, num(d)?).await?;
            Ok((StatusCode::ACCEPTED, axum::Json(json!({ "delivery": n }))).into_response())
        }
        [id, "ping"] | [id, "pings"] if m == Method::POST => {
            let h = c.flux.hook_in(&c.who, scope, num(id)?).await?;
            c.flux.ping_hook(&h).await?;
            no_content()
        }
        _ => Err(not_found("no such endpoint")),
    }
}

pub async fn protection(c: &Ctx, repo: RepoId) -> ApiResult {
    match c.method {
        Method::GET => {
            c.flux.authorize(&c.who, repo, Role::Read).await?;
            let cfg = c.flux.repo_config(repo).await?;
            reply(json!({ "rules": cfg.branch_rules, "using_defaults": cfg.branch_rules.is_none() }))
        }
        Method::PUT => {
            #[derive(Deserialize)]
            struct B {
                rules: Option<Vec<crate::protect::BranchRule>>,
            }
            let b: B = c.parse()?;
            c.flux.authorize(&c.who, repo, Role::Admin).await?;
            if let Some(rules) = &b.rules {
                for r in rules {
                    if r.pattern.trim().is_empty() {
                        return Err(bad("every rule needs a pattern"));
                    }
                    if r.required_approvals > 10 {
                        return Err(bad("at most 10 required approvals"));
                    }
                }
            }
            let mut cfg = (*c.flux.repo_config(repo).await?).clone();
            cfg.branch_rules = b.rules;
            c.flux.set_repo_config(repo, &cfg).await?;
            let name = c.flux.repo_name(repo).await;
            c.flux.audit(&c.who, "repo.protection", &name, &serde_json::to_string(&cfg.branch_rules).unwrap_or_default()).await;
            reply(json!({ "rules": cfg.branch_rules }))
        }
        _ => Err(method_not_allowed()),
    }
}

// ───────────── releases ─────────────

pub async fn releases(c: &Ctx, repo: RepoId, t: &[&str]) -> ApiResult {
    let m = &c.method;
    match t {
        [] if m == Method::GET => reply(c.flux.list_releases_as(&c.who, repo).await?),
        [] if m == Method::POST => {
            let input: crate::repos::ReleaseInput = c.parse()?;
            created(c.flux.create_release(&c.who, repo, input).await?)
        }
        ["latest"] => {
            let list = c.flux.list_releases_as(&c.who, repo).await?;
            reply(list.into_iter().find(|r| !r.draft && !r.prerelease).ok_or_else(|| not_found("no releases"))?)
        }
        ["tags", tag] => {
            let r = c.flux.release_by_tag_as(&c.who, repo, tag).await?;
            reply(c.flux.release_json(repo, &r).await?)
        }
        [id] if m == Method::GET => {
            let role = c.flux.authorize(&c.who, repo, Role::Read).await?;
            let r = c
                .flux
                .meta
                .get_release(repo, num(id)?)
                .await?
                .filter(|r| !r.draft || role >= Role::Write)
                .ok_or_else(|| not_found("release"))?;
            reply(c.flux.release_json(repo, &r).await?)
        }
        [id] if m == Method::PATCH => {
            let input: crate::repos::ReleaseInput = c.parse()?;
            reply(c.flux.update_release(&c.who, repo, num(id)?, input).await?)
        }
        [id] if m == Method::DELETE => {
            c.flux.delete_release(&c.who, repo, num(id)?).await?;
            no_content()
        }
        [id, "assets"] if m == Method::POST => {
            let name = c.q("name").ok_or_else(|| bad("pass ?name=<file name>"))?;
            let ctype = c.headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("application/octet-stream");
            created(c.flux.upload_asset(&c.who, repo, num(id)?, name, ctype, c.body.clone()).await?)
        }
        [id, "assets", aid] if m == Method::DELETE => {
            c.flux.delete_asset(&c.who, repo, num(id)?, num(aid)?).await?;
            no_content()
        }
        ["download", tag, name] => {
            let (a, data) = c.flux.download_asset(&c.who, repo, tag, name).await?;
            let mut r = data.into_response();
            let h = r.headers_mut();
            if let Ok(v) = HeaderValue::from_str(&a.content_type) {
                h.insert(header::CONTENT_TYPE, v);
            }
            if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{}\"", a.name.replace('"', ""))) {
                h.insert(header::CONTENT_DISPOSITION, v);
            }
            Ok(r)
        }
        _ => Err(not_found("no such endpoint")),
    }
}

pub async fn forks(c: &Ctx, repo: RepoId) -> ApiResult {
    match c.method {
        Method::GET => {
            c.flux.authorize(&c.who, repo, Role::Read).await?;
            let mut out = Vec::new();
            for f in c.flux.meta.forks(repo).await? {
                if c.flux.repo_role(&c.who, f).await? >= Role::Read {
                    out.push(c.flux.repo_name(f).await);
                }
            }
            reply(out)
        }
        Method::POST => {
            #[derive(Deserialize, Default)]
            struct B {
                #[serde(default)]
                name: Option<String>,
            }
            let b: B = c.parse().unwrap_or_default();
            let me = c.user()?.name.clone();
            let src = c.flux.repo_name(repo).await;
            let short = src.rsplit('/').next().unwrap_or(&src);
            let dest = b.name.unwrap_or_else(|| format!("{me}/{short}"));
            let id = c.flux.fork_repo(&c.who, repo, &dest).await?;
            created(json!({ "full_name": dest, "id": id.0, "fork_of": src }))
        }
        _ => Err(method_not_allowed()),
    }
}

pub async fn secrets(c: &Ctx, scope: (&str, i64), t: &[&str]) -> ApiResult {
    let m = &c.method;
    match t {
        [] if m == Method::GET => reply(c.flux.list_secrets(&c.who, scope).await?),
        [name] if m == Method::PUT || m == Method::POST => {
            #[derive(Deserialize)]
            struct B {
                value: String,
            }
            let b: B = c.parse()?;
            c.flux.put_secret(&c.who, scope, name, &b.value).await?;
            no_content()
        }
        [name] if m == Method::DELETE => {
            c.flux.delete_secret(&c.who, scope, name).await?;
            no_content()
        }
        _ => Err(not_found("no such endpoint")),
    }
}
