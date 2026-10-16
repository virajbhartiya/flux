//! Webhooks: management, payloads and durable delivery.
//!
//! Events are turned into GitHub-shaped JSON payloads (`push`,
//! `pull_request`, `issues`, `issue_comment`, `status`, `release`,
//! `workflow_run`, `ping`) so existing receivers mostly work unchanged.
//! Deliveries are queued in the metadata store and sent by a worker with
//! retries and exponential backoff; they survive restarts, and concurrent
//! workers (several server instances) never send the same attempt twice.
//!
//! Each request carries `X-Flux-Event`/`X-GitHub-Event`,
//! `X-Flux-Delivery`/`X-GitHub-Delivery` and, when the hook has a secret,
//! `X-Hub-Signature-256: sha256=<hmac>` over the body.
//!
//! Targets on loopback, private, link-local or metadata addresses are
//! refused unless `webhook_allow_private` is set, and the address checked is
//! the one connected to (no DNS rebinding, no redirects).

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use flux_store::{Delivery, RepoId, Webhook};
use hmac::Mac as _;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::auth::{Principal, now_ms};
use crate::core::{Event, Flux, FluxError, FluxResult};
use crate::rbac::Role;

pub const EVENTS: [&str; 9] =
    ["push", "pull_request", "pull_request_review", "issues", "issue_comment", "status", "release", "workflow_run", "ping"];

/// Attempts before a delivery is given up.
const MAX_ATTEMPTS: i64 = 8;
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const CONCURRENCY: usize = 16;

/// Where a hook is installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Repo(RepoId),
    Org(i64),
}

impl Scope {
    fn parts(self) -> (&'static str, i64) {
        match self {
            Scope::Repo(r) => ("repo", r.0),
            Scope::Org(o) => ("org", o),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct HookInput {
    pub url: String,
    #[serde(default)]
    pub secret: Option<String>,
    #[serde(default)]
    pub events: Option<Vec<String>>,
    #[serde(default)]
    pub active: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
pub struct HookJson {
    pub id: i64,
    pub url: String,
    pub events: Vec<String>,
    pub active: bool,
    pub has_secret: bool,
    pub created_at: i64,
}

impl From<&Webhook> for HookJson {
    fn from(h: &Webhook) -> Self {
        Self {
            id: h.id,
            url: h.url.clone(),
            events: h.events.clone(),
            active: h.active,
            has_secret: !h.secret.is_empty(),
            created_at: h.created_ms,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct DeliveryJson {
    pub id: i64,
    pub guid: String,
    pub event: String,
    pub state: String,
    pub attempts: i64,
    pub status_code: i64,
    pub duration_ms: i64,
    pub created_at: i64,
    pub next_attempt_at: Option<i64>,
    pub response: String,
    pub payload: Value,
}

impl From<&Delivery> for DeliveryJson {
    fn from(d: &Delivery) -> Self {
        Self {
            id: d.id,
            guid: d.guid.clone(),
            event: d.event.clone(),
            state: d.state.clone(),
            attempts: d.attempts,
            status_code: d.status_code,
            duration_ms: d.duration_ms,
            created_at: d.created_ms,
            next_attempt_at: (d.state == "pending").then_some(d.next_ms),
            response: d.response.clone(),
            payload: serde_json::from_str(&d.payload).unwrap_or(Value::Null),
        }
    }
}

fn guid() -> String {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).expect("OS random number generator");
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = hex::encode(b);
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

pub fn sign(secret: &str, body: &[u8]) -> String {
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()).expect("any key length");
    mac.update(body);
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

/// Seconds to wait after the n-th failed attempt.
fn backoff_secs(attempts: i64) -> i64 {
    match attempts {
        ..=1 => 10,
        2 => 30,
        3 => 120,
        4 => 600,
        5 => 1800,
        6 => 3600,
        _ => 10_800,
    }
}

fn forbidden_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_documentation()
                || (o[0] == 100 && (o[1] & 0xc0) == 64) // 100.64.0.0/10
                || o[0] == 0
                || o[0] >= 240
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return forbidden_ip(IpAddr::V4(v4));
            }
            let s = v6.segments();
            v6.is_loopback() || v6.is_unspecified() || (s[0] & 0xfe00) == 0xfc00 || (s[0] & 0xffc0) == 0xfe80
        }
    }
}

pub fn check_url(url: &str) -> FluxResult<url::Url> {
    let u = url::Url::parse(url.trim()).map_err(|e| FluxError::InvalidArgument(format!("invalid URL: {e}")))?;
    if !matches!(u.scheme(), "http" | "https") || u.host_str().is_none() {
        return Err(FluxError::InvalidArgument("webhook URLs must be http(s) with a host".into()));
    }
    if !u.username().is_empty() || u.password().is_some() {
        return Err(FluxError::InvalidArgument("put credentials in the secret, not the URL".into()));
    }
    Ok(u)
}

impl Flux {
    async fn hook_scope_admin(&self, who: &Principal, scope: Scope) -> FluxResult<()> {
        match scope {
            Scope::Repo(r) => {
                self.authorize(who, r, Role::Admin).await?;
            }
            Scope::Org(o) => {
                let org =
                    self.meta.get_org_by_id(o).await?.ok_or_else(|| FluxError::NotFound("organization not found".into()))?;
                if !self.is_org_owner(who, &org).await? {
                    return Err(FluxError::PermissionDenied("only organization owners manage its webhooks".into()));
                }
            }
        }
        Ok(())
    }

    fn check_events(events: &[String]) -> FluxResult<()> {
        for e in events {
            if e != "*" && !EVENTS.contains(&e.as_str()) {
                return Err(FluxError::InvalidArgument(format!("unknown event {e:?} (one of {} or *)", EVENTS.join(", "))));
            }
        }
        Ok(())
    }

    pub async fn create_hook(&self, who: &Principal, scope: Scope, input: HookInput) -> FluxResult<HookJson> {
        self.hook_scope_admin(who, scope).await?;
        let url = check_url(&input.url)?;
        let events = input.events.unwrap_or_else(|| vec!["push".into()]);
        Self::check_events(&events)?;
        let (kind, id) = scope.parts();
        let h = self
            .meta
            .create_webhook(&Webhook {
                id: 0,
                scope_kind: kind.into(),
                scope_id: id,
                url: url.to_string(),
                secret: input.secret.unwrap_or_default(),
                events,
                active: input.active.unwrap_or(true),
                created_ms: now_ms(),
            })
            .await?;
        self.audit(who, "hook.create", &format!("{kind}:{id}"), &h.url).await;
        self.ping_hook(&h).await?;
        Ok(HookJson::from(&h))
    }

    /// The hook if it belongs to `scope` (hooks are addressed by id).
    pub async fn hook_in(&self, who: &Principal, scope: Scope, id: i64) -> FluxResult<Webhook> {
        self.hook_scope_admin(who, scope).await?;
        let (kind, sid) = scope.parts();
        self.meta
            .get_webhook(id)
            .await?
            .filter(|h| h.scope_kind == kind && h.scope_id == sid)
            .ok_or_else(|| FluxError::NotFound(format!("hook {id} not found")))
    }

    pub async fn list_hooks(&self, who: &Principal, scope: Scope) -> FluxResult<Vec<HookJson>> {
        self.hook_scope_admin(who, scope).await?;
        let (kind, id) = scope.parts();
        Ok(self.meta.list_webhooks(kind, id).await?.iter().map(HookJson::from).collect())
    }

    pub async fn update_hook(&self, who: &Principal, scope: Scope, id: i64, input: HookInput) -> FluxResult<HookJson> {
        let mut h = self.hook_in(who, scope, id).await?;
        if !input.url.is_empty() {
            h.url = check_url(&input.url)?.to_string();
        }
        if let Some(s) = input.secret {
            h.secret = s;
        }
        if let Some(e) = input.events {
            Self::check_events(&e)?;
            h.events = e;
        }
        if let Some(a) = input.active {
            h.active = a;
        }
        self.meta.update_webhook(&h).await?;
        self.audit(who, "hook.update", &id.to_string(), &h.url).await;
        Ok(HookJson::from(&h))
    }

    pub async fn delete_hook(&self, who: &Principal, scope: Scope, id: i64) -> FluxResult<()> {
        self.hook_in(who, scope, id).await?;
        self.meta.delete_webhook(id).await?;
        self.audit(who, "hook.delete", &id.to_string(), "").await;
        Ok(())
    }

    pub async fn hook_deliveries(&self, who: &Principal, scope: Scope, id: i64) -> FluxResult<Vec<DeliveryJson>> {
        self.hook_in(who, scope, id).await?;
        Ok(self.meta.list_deliveries(id, 100).await?.iter().map(DeliveryJson::from).collect())
    }

    pub async fn redeliver(&self, who: &Principal, scope: Scope, id: i64, delivery: i64) -> FluxResult<i64> {
        self.hook_in(who, scope, id).await?;
        let d = self
            .meta
            .get_delivery(delivery)
            .await?
            .filter(|d| d.hook_id == id)
            .ok_or_else(|| FluxError::NotFound("delivery not found".into()))?;
        let n = self.meta.enqueue_delivery(id, &d.event, &guid(), &d.payload).await?;
        self.hook_wake.notify_one();
        Ok(n)
    }

    pub async fn ping_hook(&self, h: &Webhook) -> FluxResult<()> {
        let payload = json!({
            "zen": "Fast is a feature.",
            "hook_id": h.id,
            "hook": { "id": h.id, "type": if h.scope_kind == "org" { "Organization" } else { "Repository" }, "events": h.events, "active": h.active, "config": { "url": h.url, "content_type": "json" } },
        });
        self.meta.enqueue_delivery(h.id, "ping", &guid(), &payload.to_string()).await?;
        self.hook_wake.notify_one();
        Ok(())
    }

    /// Queue an event for every matching hook of the repository and its
    /// owning organization.
    pub(crate) async fn fan_out_hooks(&self, repo: RepoId, event: &str, payload: &Value) -> FluxResult<usize> {
        let mut hooks = self.meta.list_webhooks("repo", repo.0).await?;
        let access = self.repo_access(repo).await?;
        if access.owner_kind == "org" {
            hooks.extend(self.meta.list_webhooks("org", access.owner_id).await?);
        }
        let body = payload.to_string();
        let mut n = 0;
        for h in hooks.iter().filter(|h| h.active && h.events.iter().any(|e| e == "*" || e == event)) {
            self.meta.enqueue_delivery(h.id, event, &guid(), &body).await?;
            n += 1;
        }
        if n > 0 {
            self.hook_wake.notify_one();
        }
        Ok(n)
    }

    // ───────────── payloads ─────────────

    pub(crate) async fn repo_json(&self, repo: RepoId) -> Value {
        let name = self.repo_name(repo).await;
        let base = self.settings.base_url();
        let cfg = self.repo_config(repo).await.ok();
        let access = self.repo_access(repo).await.ok();
        let (owner, short) = name.split_once('/').unwrap_or(("", &name));
        json!({
            "id": repo.0,
            "name": short,
            "full_name": name,
            "owner": { "login": owner },
            "private": access.as_ref().is_some_and(|a| a.visibility != "public"),
            "html_url": format!("{base}/{name}"),
            "clone_url": format!("{base}/{name}.git"),
            "default_branch": cfg.as_ref().and_then(|c| c.default_branch.clone()).unwrap_or_else(|| "main".into()),
            "description": cfg.as_ref().map(|c| c.description.clone()).unwrap_or_default(),
        })
    }

    async fn git_sha_hex(&self, repo: RepoId, cs: &flux_core::ChangesetId) -> String {
        match self.git_commit_shas(repo, std::slice::from_ref(cs)).await {
            Ok(v) if !v.is_empty() => hex::encode(v[0]),
            _ => String::new(),
        }
    }

    async fn review_json(&self, repo: RepoId, number: i64) -> FluxResult<Value> {
        let r = self.review(repo, number).await?;
        let (draft, _) = self.meta.review_flags(repo, number).await?;
        let base = self.settings.base_url();
        let name = self.repo_name(repo).await;
        let labels = self.meta.item_labels(repo, &[number]).await?;
        Ok(json!({
            "number": r.number,
            "title": r.title,
            "body": r.body,
            "state": if r.state == "open" { "open" } else { "closed" },
            "merged": r.state == "landed",
            "draft": draft,
            "user": { "login": r.author },
            "html_url": format!("{base}/{name}/-/reviews/{number}"),
            "head": { "sha": self.git_sha_hex(repo, &r.head).await, "changeset": r.head.to_hex() },
            "base": { "ref": String::from_utf8_lossy(&r.target) },
            "merge_commit_sha": match r.landed { Some(l) => self.git_sha_hex(repo, &l).await, None => String::new() },
            "labels": labels.iter().map(|(_, l)| json!({ "name": l })).collect::<Vec<_>>(),
            "created_at": r.created_ms,
            "updated_at": r.updated_ms,
        }))
    }

    async fn issue_payload(&self, repo: RepoId, number: i64) -> FluxResult<Value> {
        let i = self.meta.get_issue(repo, number).await?.ok_or_else(|| FluxError::NotFound("issue".into()))?;
        let j = self.issues_json(repo, &[i]).await?.remove(0);
        let base = self.settings.base_url();
        let name = self.repo_name(repo).await;
        Ok(json!({
            "number": j.number,
            "title": j.title,
            "body": j.body,
            "state": j.state,
            "state_reason": j.state_reason,
            "user": { "login": j.author },
            "labels": j.labels.iter().map(|l| json!({ "name": l })).collect::<Vec<_>>(),
            "assignees": j.assignees.iter().map(|a| json!({ "login": a })).collect::<Vec<_>>(),
            "milestone": j.milestone,
            "comments": j.comments,
            "html_url": format!("{base}/{name}/-/issues/{number}"),
            "created_at": j.created_at,
            "updated_at": j.updated_at,
            "closed_at": j.closed_at,
        }))
    }

    /// The webhook event name and payload for an event, if it has one.
    pub(crate) async fn hook_payload(&self, e: &Event) -> FluxResult<Option<(&'static str, RepoId, Value)>> {
        let repo = match self.repo(e.repo()).await {
            Ok(r) => r,
            Err(_) => return Ok(None),
        };
        let repository = self.repo_json(repo).await;
        Ok(Some(match e {
            Event::RefMoved { name, old, new, actor, .. } => {
                let n = String::from_utf8_lossy(name).to_string();
                if n.starts_with("pull/") || n.starts_with("review/") || n.starts_with("flux/") {
                    return Ok(None);
                }
                let git_ref = match n.strip_prefix("tags/") {
                    Some(t) => format!("refs/tags/{t}"),
                    None => format!("refs/heads/{n}"),
                };
                let zero = "0".repeat(40);
                let before = match old {
                    Some(o) => self.git_sha_hex(repo, o).await,
                    None => zero.clone(),
                };
                let after = match new {
                    Some(c) => self.git_sha_hex(repo, c).await,
                    None => zero.clone(),
                };
                let mut commits = Vec::new();
                if let Some(head) = new {
                    let hide: Vec<_> = old.iter().copied().collect();
                    let page = self.log_page(repo, head, &hide, None, false, 0, 20).await.unwrap_or_default();
                    let base = self.settings.base_url();
                    let rname = self.repo_name(repo).await;
                    for l in page.iter().rev() {
                        let sha = self.git_sha_hex(repo, &l.id).await;
                        let (an, ae) = split_sig(&l.changeset.author);
                        commits.push(json!({
                            "id": sha,
                            "changeset": l.id.to_hex(),
                            "message": String::from_utf8_lossy(&l.changeset.message).trim_end(),
                            "timestamp": l.changeset.author_time.unix_ms,
                            "url": format!("{base}/{rname}/-/commit/{}", l.id.to_hex()),
                            "author": { "name": an, "email": ae },
                            "added": [], "removed": [],
                            "modified": l.changeset.file_changes.keys().take(100).map(|p| p.to_string()).collect::<Vec<_>>(),
                        }));
                    }
                }
                let head_commit = commits.last().cloned().unwrap_or(Value::Null);
                (
                    "push",
                    repo,
                    json!({
                        "ref": git_ref,
                        "before": before,
                        "after": after,
                        "created": old.is_none(),
                        "deleted": new.is_none(),
                        "forced": false,
                        "commits": commits,
                        "head_commit": head_commit,
                        "repository": repository,
                        "pusher": { "name": actor },
                        "sender": { "login": actor },
                    }),
                )
            }
            Event::Review { number, kind, actor, .. } => {
                let pr = self.review_json(repo, *number).await?;
                match kind.as_str() {
                    "approved" | "changes_requested" => (
                        "pull_request_review",
                        repo,
                        json!({
                            "action": "submitted",
                            "review": { "state": kind, "user": { "login": actor } },
                            "pull_request": pr,
                            "repository": repository,
                            "sender": { "login": actor },
                        }),
                    ),
                    "comment" => {
                        let ev = self.meta.review_events(repo, *number).await?;
                        let c = ev.iter().rev().find(|e| e.kind == "comment" && &e.author == actor);
                        (
                            "issue_comment",
                            repo,
                            json!({
                                "action": "created",
                                "issue": { "number": number, "title": pr["title"], "pull_request": { "html_url": pr["html_url"] } },
                                "comment": c.map(|c| json!({ "id": c.seq, "body": c.body, "user": { "login": c.author },
                                    "path": c.path.as_ref().map(|p| String::from_utf8_lossy(p).to_string()), "line": c.line })),
                                "repository": repository,
                                "sender": { "login": actor },
                            }),
                        )
                    }
                    _ => {
                        let action = match kind.as_str() {
                            "opened" => "opened",
                            "updated" => "synchronize",
                            "landed" | "closed" => "closed",
                            "reopened" => "reopened",
                            "ready" => "ready_for_review",
                            other => other,
                        };
                        (
                            "pull_request",
                            repo,
                            json!({ "action": action, "number": number, "pull_request": pr, "repository": repository, "sender": { "login": actor } }),
                        )
                    }
                }
            }
            Event::Issue { number, action, actor, .. } => (
                "issues",
                repo,
                json!({ "action": action, "issue": self.issue_payload(repo, *number).await?, "repository": repository, "sender": { "login": actor } }),
            ),
            Event::Comment { number, on: "issue", seq, actor, body, .. } => (
                "issue_comment",
                repo,
                json!({
                    "action": "created",
                    "issue": self.issue_payload(repo, *number).await?,
                    "comment": { "id": seq, "body": body, "user": { "login": actor } },
                    "repository": repository,
                    "sender": { "login": actor },
                }),
            ),
            Event::Status { cs, context, state, actor, .. } => {
                let st = self.meta.statuses(repo, cs).await?;
                let s = st.iter().find(|s| &s.context == context);
                (
                    "status",
                    repo,
                    json!({
                        "sha": self.git_sha_hex(repo, cs).await,
                        "changeset": cs.to_hex(),
                        "state": state,
                        "context": context,
                        "description": s.map(|s| s.description.clone()),
                        "target_url": s.map(|s| s.target_url.clone()),
                        "repository": repository,
                        "sender": { "login": actor },
                    }),
                )
            }
            Event::Release { id, action, actor, .. } => {
                let r = self.meta.get_release(repo, *id).await?;
                (
                    "release",
                    repo,
                    json!({
                        "action": action,
                        "release": r.map(|r| json!({ "id": r.id, "tag_name": r.tag, "name": r.name, "body": r.body, "draft": r.draft, "prerelease": r.prerelease, "author": { "login": r.author } })),
                        "repository": repository,
                        "sender": { "login": actor },
                    }),
                )
            }
            Event::Ci { run, state, .. } => {
                let r = self.meta.get_ci_run(*run).await?;
                let done = matches!(state.as_str(), "success" | "failure" | "cancelled");
                (
                    "workflow_run",
                    repo,
                    json!({
                        "action": if done { "completed" } else if state == "queued" { "requested" } else { "in_progress" },
                        "workflow_run": r.map(|r| json!({ "id": r.id, "name": r.workflow, "event": r.event, "head_branch": r.ref_name,
                            "status": if done { "completed" } else { state.as_str() }, "conclusion": if done { Some(state.clone()) } else { None } })),
                        "repository": repository,
                    }),
                )
            }
            Event::Comment { .. } | Event::Changesets { .. } => return Ok(None),
        }))
    }
}

/// `Name <email>` → (name, email).
pub fn split_sig(sig: &[u8]) -> (String, String) {
    let s = String::from_utf8_lossy(sig);
    match (s.find('<'), s.rfind('>')) {
        (Some(a), Some(b)) if a < b => (s[..a].trim().to_string(), s[a + 1..b].trim().to_string()),
        _ => (s.trim().to_string(), String::new()),
    }
}

// ───────────── delivery worker ─────────────

/// Resolve and vet the target; returns the address to connect to.
async fn resolve_target(u: &url::Url, allow_private: bool) -> Result<SocketAddr, String> {
    let host = u.host_str().ok_or("no host")?.trim_start_matches('[').trim_end_matches(']').to_string();
    let port = u.port_or_known_default().ok_or("no port")?;
    let addrs: Vec<SocketAddr> =
        tokio::net::lookup_host((host.as_str(), port)).await.map_err(|e| format!("cannot resolve {host}: {e}"))?.collect();
    let pick = addrs.into_iter().find(|a| allow_private || !forbidden_ip(a.ip()));
    pick.ok_or_else(|| format!("{host} resolves only to private or reserved addresses"))
}

async fn send_one(flux: &Flux, d: &Delivery) -> (bool, i64, String, i64) {
    let started = std::time::Instant::now();
    let Ok(Some(hook)) = flux.meta.get_webhook(d.hook_id).await else {
        return (false, 0, "hook deleted".into(), 0);
    };
    if !hook.active && d.event != "ping" {
        return (false, 0, "hook inactive".into(), 0);
    }
    let u = match check_url(&hook.url) {
        Ok(u) => u,
        Err(e) => return (false, 0, e.to_string(), 0),
    };
    let addr = match resolve_target(&u, flux.settings.webhook_allow_private).await {
        Ok(a) => a,
        Err(e) => return (false, 0, e, 0),
    };
    let host = u.host_str().unwrap_or_default().to_string();
    let client = match reqwest::Client::builder()
        .timeout(TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .resolve(&host, addr)
        .user_agent("flux-hookshot/1")
        .build()
    {
        Ok(c) => c,
        Err(e) => return (false, 0, e.to_string(), 0),
    };
    let body = d.payload.clone().into_bytes();
    let mut req = client
        .post(u)
        .header("content-type", "application/json")
        .header("x-flux-event", &d.event)
        .header("x-github-event", &d.event)
        .header("x-flux-delivery", &d.guid)
        .header("x-github-delivery", &d.guid)
        .header("x-flux-hook-id", hook.id.to_string());
    if !hook.secret.is_empty() {
        req = req.header("x-hub-signature-256", sign(&hook.secret, &body));
    }
    match req.body(body).send().await {
        Ok(resp) => {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            let text: String = text.chars().take(4096).collect();
            (status.is_success(), status.as_u16() as i64, text, started.elapsed().as_millis() as i64)
        }
        Err(e) => (false, 0, e.to_string(), started.elapsed().as_millis() as i64),
    }
}

/// Start the delivery worker (called once by the reactor).
pub(crate) fn start_worker(flux: Arc<Flux>) {
    let sem = Arc::new(tokio::sync::Semaphore::new(CONCURRENCY));
    tokio::spawn(async move {
        let mut last_prune = 0i64;
        loop {
            let now = now_ms();
            if now - last_prune > 3_600_000 {
                last_prune = now;
                let _ = flux.meta.prune_deliveries(now - 30 * 86_400_000).await;
            }
            let claimed = match flux.meta.claim_deliveries(now, now + 120_000, 32).await {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(error = %e, "webhooks: claim failed");
                    vec![]
                }
            };
            if claimed.is_empty() {
                tokio::select! {
                    _ = flux.hook_wake.notified() => {}
                    _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => {}
                }
                continue;
            }
            for d in claimed {
                let permit = sem.clone().acquire_owned().await.expect("semaphore open");
                let flux = flux.clone();
                tokio::spawn(async move {
                    let (ok, code, response, ms) = send_one(&flux, &d).await;
                    let (state, next) = if ok {
                        ("ok", 0)
                    } else if d.attempts >= MAX_ATTEMPTS || response == "hook deleted" {
                        ("failed", 0)
                    } else {
                        ("pending", now_ms() + backoff_secs(d.attempts) * 1000)
                    };
                    if !ok {
                        tracing::info!(hook = d.hook_id, delivery = d.id, attempt = d.attempts, code, "webhook delivery failed");
                    }
                    if let Err(e) = flux.meta.finish_delivery(d.id, state, next, code, &response, ms).await {
                        tracing::warn!(error = %e, "webhooks: could not record delivery");
                    }
                    drop(permit);
                });
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signatures_match_github() {
        // From GitHub's documentation for validating webhook deliveries.
        assert_eq!(
            sign("It's a Secret to Everybody", b"Hello, World!"),
            "sha256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17"
        );
    }

    #[test]
    fn private_addresses_are_refused() {
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "192.168.0.1",
            "169.254.169.254",
            "100.64.1.1",
            "::1",
            "fd00::1",
            "fe80::1",
            "::ffff:10.0.0.1",
        ] {
            assert!(forbidden_ip(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["8.8.8.8", "2606:4700::1111"] {
            assert!(!forbidden_ip(ip.parse().unwrap()), "{ip}");
        }
        assert!(check_url("ftp://x").is_err());
        assert!(check_url("https://user:pw@x.com/").is_err());
        assert!(check_url("https://hooks.example.com/x").is_ok());
    }
}
