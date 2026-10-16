//! The notifications inbox and SSH keys (personal settings).

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use maud::html;

use super::account::{check_origin, settings_layout};
use super::forms::{Fields, done};
use super::view::{enc, icon, repo_url, time};
use super::{Req, WebError, WebResult, WebState};

fn me(req: &Req) -> WebResult<&flux_store::User> {
    req.principal.user.as_ref().ok_or_else(|| WebError::Unauthorized("sign in first".into()))
}

fn showing_all(req: &Req) -> bool {
    req.query.split('&').any(|kv| kv == "all=1")
}

fn reason_text(r: &str) -> &str {
    match r {
        "assigned" => "assigned",
        "review_requested" => "review requested",
        "mention" | "mentioned" => "mentioned",
        "author" => "author",
        "comment" => "comment",
        "subscribed" | "watching" => "subscribed",
        "state_change" => "state change",
        other => other,
    }
}

pub async fn notifications(State(web): State<WebState>, req: Req) -> WebResult {
    let u = match me(&req) {
        Ok(u) => u,
        Err(_) => return Ok(super::forms::to(&format!("/-/login?next={}", enc("/-/notifications")))),
    };
    let all = showing_all(&req);
    let list = web.flux.meta.notifications(u.id, !all, 100).await?;
    let mut rows = Vec::with_capacity(list.len());
    for n in &list {
        // Repository names are cached in memory: no query per row.
        rows.push((web.flux.repo_name(n.repo_id).await, n));
    }
    let suffix = if all { "?all=1" } else { "" };
    let content = html! {
        div.page-head.row {
            h2 { "Notifications" }
            div.spacer {}
            @if req.unread > 0 {
                form method="post" action=(format!("/-/notifications/read{suffix}")) {
                    button.btn.small type="submit" { (icon("check")) "Mark all as read" }
                }
            }
        }
        div.card {
            div.filter-tabs {
                a.active[!all] href="/-/notifications" { "Unread" span.count { (req.unread) } }
                a.active[all] href="/-/notifications?all=1" { "All" }
            }
            @if rows.is_empty() {
                div.empty.small {
                    div.empty-art.inbox-art { (icon("inbox")) }
                    h2 { @if all { "No notifications yet" } @else { "You're all caught up" } }
                    p.muted { "Watch a repository, or take part in an issue or review, to hear about it here." }
                }
            }
            @for (repo, n) in &rows {
                @let path = if n.kind == "issue" { "issues" } else { "reviews" };
                @let href = if repo.is_empty() { "#".to_string() } else { format!("{}/-/{path}/{}", repo_url(repo), n.number) };
                div.list-row.notification.unread[n.unread] {
                    span.list-icon { (icon(if n.kind == "issue" { "issue" } else { "review" })) }
                    div.list-main {
                        div.muted.small { (repo) " #" (n.number) }
                        a.note-title href=(href) { @if n.title.is_empty() { "(untitled)" } @else { (n.title) } }
                    }
                    span.pill.small { (reason_text(&n.reason)) }
                    span.muted.small.hide-sm { (time(n.updated_ms)) }
                    @if n.unread {
                        form method="post" action=(format!("/-/notifications/{}/read{suffix}", n.id)) {
                            button.btn.small.ghost.icon-only type="submit" title="Mark as read" aria-label="Mark as read" { (icon("check")) }
                        }
                    } @else {
                        span.read-spacer {}
                    }
                }
            }
        }
    };
    Ok(settings_layout(&web, &req, "Notifications", "notifications", content))
}

pub async fn mark_all_read(State(web): State<WebState>, req: Req, headers: HeaderMap) -> WebResult {
    check_origin(&headers)?;
    let u = me(&req)?;
    let back = if showing_all(&req) { "/-/notifications?all=1" } else { "/-/notifications" };
    done(back, web.flux.meta.mark_read(u.id, None).await.map(|n| format!("Marked {n} as read")))
}

pub async fn mark_read(State(web): State<WebState>, req: Req, headers: HeaderMap, Path(id): Path<i64>) -> WebResult {
    check_origin(&headers)?;
    let u = me(&req)?;
    let back = if showing_all(&req) { "/-/notifications?all=1" } else { "/-/notifications" };
    done(back, web.flux.meta.mark_read(u.id, Some(id)).await.map(|_| String::new()))
}

// ───────────── SSH keys ─────────────

pub async fn ssh_keys(State(web): State<WebState>, req: Req) -> WebResult {
    let u = me(&req)?;
    let keys = web.flux.meta.list_ssh_keys(u.id).await?;
    let ssh = web.flux.settings.ssh_host.clone();
    let content = html! {
        div.page-head { h2 { "SSH keys" } }
        p.muted {
            "Keys let " code { "git" } " push and pull over SSH without a password."
            @if let Some(h) = &ssh { " Clone with " code { "git clone ssh://git@" (h) "/OWNER/REPO.git" } "." }
            @else { " SSH isn't enabled on this server yet." }
        }
        div.card {
            div.card-head { (icon("key")) "Your keys" span.count { (keys.len()) } }
            @if keys.is_empty() { div.pad.muted { "No SSH keys yet." } }
            @for k in &keys {
                div.list-row {
                    span.list-icon { (icon("key")) }
                    div.list-main {
                        strong { (k.title) }
                        div.mono.small.fingerprint { (k.fingerprint) }
                        div.muted.small {
                            "Added " (time(k.created_ms))
                            @match k.last_used { Some(t) => { " · last used " (time(t)) } None => { " · never used" } }
                        }
                    }
                    form method="post" action=(format!("/-/settings/keys/{}/delete", k.id)) data-confirm=(format!("Delete the key {:?}?", k.title)) {
                        button.btn.small.danger type="submit" { "Delete" }
                    }
                }
            }
        }
        div.card {
            div.card-head { (icon("plus")) "Add a key" }
            form.form method="post" action="/-/settings/keys" {
                label { "Title" input type="text" name="title" maxlength="100" placeholder="Work laptop"; }
                label { "Key" span.hint { "Begins with " code { "ssh-ed25519" } ", " code { "ecdsa-sha2-nistp256" } " or " code { "ssh-rsa" } ". Usually in " code { "~/.ssh/id_ed25519.pub" } "." }
                    textarea.mono name="key" rows="4" required spellcheck="false" placeholder="ssh-ed25519 AAAA… you@laptop" {}
                }
                div.form-actions { button.btn.primary type="submit" { "Add SSH key" } }
            }
        }
    };
    Ok(settings_layout(&web, &req, "SSH keys", "keys", content))
}

pub async fn ssh_key_add(State(web): State<WebState>, req: Req, headers: HeaderMap, body: Body) -> WebResult {
    check_origin(&headers)?;
    me(&req)?;
    let f = Fields::read(&headers, body).await?;
    done(
        "/-/settings/keys",
        web.flux.add_ssh_key(&req.principal, f.trim("title"), f.trim("key")).await.map(|k| format!("Added {}", k.fingerprint)),
    )
}

pub async fn ssh_key_delete(State(web): State<WebState>, req: Req, headers: HeaderMap, Path(id): Path<i64>) -> WebResult {
    check_origin(&headers)?;
    me(&req)?;
    done("/-/settings/keys", web.flux.delete_ssh_key(&req.principal, id).await.map(|_| "Key deleted".to_string()))
}
