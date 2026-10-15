//! Accounts, organizations, teams, repository access and site admin pages.

use axum::extract::{Form, Path, State};
use axum::http::{HeaderMap, HeaderValue, header};
use axum::response::{IntoResponse, Redirect, Response};
use flux_store::{Org, RepoId};
use maud::{Markup, html};
use serde::Deserialize;

use super::view::{avatar, empty_state, enc, icon, page, repo_url, time};
use super::{COOKIE, Req, WebError, WebResult, WebState};
use crate::auth::Principal;
use crate::rbac::{ROLES, Role};

/// Every account form posts a subset of these fields.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct AccountForm {
    pub name: String,
    pub email: String,
    pub display_name: String,
    pub password: String,
    pub password2: String,
    pub old_password: String,
    pub login: String,
    pub token: String,
    pub label: String,
    pub scopes: String,
    pub expires: String,
    pub role: String,
    pub user: String,
    pub action: String,
    pub visibility: String,
    pub description: String,
    pub base_role: String,
    pub kind: String,
    pub subject: String,
    pub owner: String,
    pub next: String,
}

/// CSRF defence in depth (cookies are SameSite=Lax).
pub fn check_origin(headers: &HeaderMap) -> WebResult<()> {
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok()).unwrap_or("");
    if let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok())
        && origin != "null"
    {
        let o = origin.split_once("://").map(|(_, h)| h).unwrap_or(origin);
        if o != host {
            return Err(WebError::Forbidden("cross-origin request".into()));
        }
    }
    Ok(())
}

fn safe_next(next: &str) -> &str {
    if next.starts_with('/') && !next.starts_with("//") && !next.contains('\\') { next } else { "/" }
}

fn query(req: &Req, key: &str) -> Option<String> {
    req.query.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == key).then(|| super::pages::decode_repo(v))
    })
}

fn session_cookie(headers: &HeaderMap, token: &str, max_age_s: i64) -> WebResult<HeaderValue> {
    let secure = headers.get("x-forwarded-proto").and_then(|v| v.to_str().ok()) == Some("https");
    HeaderValue::from_str(&format!(
        "{COOKIE}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age_s}{}",
        if secure { "; Secure" } else { "" }
    ))
    .map_err(|e| WebError::BadRequest(e.to_string()))
}

fn client_of(headers: &HeaderMap) -> String {
    headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "local".into())
}

fn me(req: &Req) -> WebResult<&flux_store::User> {
    req.principal.user.as_ref().ok_or_else(|| WebError::Unauthorized("sign in first".into()))
}

fn notice(req: &Req) -> Markup {
    html! {
        @if let Some(m) = query(req, "ok") { div.notice.ok { (icon("check")) (m) } }
        @if let Some(m) = query(req, "err") { div.notice.error { (icon("alert")) (m) } }
    }
}

fn back(to: &str, msg: &str) -> Response {
    Redirect::to(&format!("{to}{}ok={}", if to.contains('?') { "&" } else { "?" }, enc(msg))).into_response()
}

fn role_select(name: &str, selected: &str, options: &[&str]) -> Markup {
    html! {
        select name=(name) {
            @for o in options {
                option value=(o) selected[*o == selected] { (o) }
            }
        }
    }
}

// ───────────── sign up / sign in ─────────────

pub async fn signup_form(State(web): State<WebState>, req: Req) -> WebResult {
    let first = web.flux.meta.user_count().await? == 0;
    let open = first || web.flux.signup == crate::identity::SignupPolicy::Open;
    let body = html! {
        div.container.narrow {
            div.card.login {
                h1 { @if first { "Create the admin account" } @else { "Create your account" } }
                @if first { p.muted { "This is the first account on this server, so it becomes the site admin." } }
                (notice(&req))
                @if open {
                    form method="post" action="/-/signup" {
                        label { "Username" input type="text" name="name" required autofocus autocomplete="username" pattern="[A-Za-z0-9][A-Za-z0-9._\\-]*" maxlength="39"; }
                        label { "Email" span.hint { "Optional. Used to sign in." } input type="text" name="email" autocomplete="email"; }
                        label { "Password" span.hint { "At least 10 characters." } input type="password" name="password" required minlength="10" autocomplete="new-password"; }
                        label { "Confirm password" input type="password" name="password2" required minlength="10" autocomplete="new-password"; }
                        button.btn.primary.wide type="submit" { "Create account" }
                    }
                } @else {
                    p { "Sign-up is closed on this server. Ask an administrator for an account." }
                }
                p.muted.small.center { "Already have an account? " a href="/-/login" { "Sign in" } }
            }
        }
    };
    Ok(page(&web, &req, "Sign up", None, body))
}

pub async fn signup(State(web): State<WebState>, headers: HeaderMap, Form(f): Form<AccountForm>) -> WebResult {
    check_origin(&headers)?;
    if f.password != f.password2 {
        return Ok(Redirect::to(&format!("/-/signup?err={}", enc("The passwords don't match"))).into_response());
    }
    let email = Some(f.email.as_str()).filter(|e| !e.trim().is_empty());
    if let Err(e) = web.flux.sign_up(f.name.trim(), email, &f.display_name, &f.password).await {
        return Ok(Redirect::to(&format!("/-/signup?err={}", enc(&e.to_string()))).into_response());
    }
    let (_, token) = web.flux.password_login(f.name.trim(), &f.password, &client_of(&headers)).await?;
    let mut r = Redirect::to("/").into_response();
    r.headers_mut().insert(header::SET_COOKIE, session_cookie(&headers, &token, crate::identity::SESSION_DAYS * 86_400)?);
    Ok(r)
}

pub async fn login_form(State(web): State<WebState>, req: Req) -> WebResult {
    let next = query(&req, "next").unwrap_or_else(|| "/".into());
    let first = web.flux.meta.user_count().await? == 0;
    let can_signup = first || web.flux.signup == crate::identity::SignupPolicy::Open;
    let body = html! {
        div.container.narrow {
            div.card.login {
                h1 { "Sign in to flux" }
                (notice(&req))
                form method="post" action="/-/login" {
                    input type="hidden" name="next" value=(next);
                    label { "Username or email" input type="text" name="login" required autofocus autocomplete="username"; }
                    label { "Password" input type="password" name="password" required autocomplete="current-password"; }
                    button.btn.primary.wide type="submit" { "Sign in" }
                }
                details.alt-login {
                    summary { "Use an access token instead" }
                    form method="post" action="/-/login" {
                        input type="hidden" name="next" value=(next);
                        label { "Token" input type="password" name="token" placeholder="flx_…" autocomplete="off"; }
                        button.btn.wide type="submit" { "Sign in with token" }
                    }
                }
                @if can_signup { p.muted.small.center { "New here? " a href="/-/signup" { @if first { "Create the admin account" } @else { "Create an account" } } } }
            }
        }
    };
    Ok(page(&web, &req, "Sign in", None, body))
}

pub async fn login(State(web): State<WebState>, headers: HeaderMap, Form(f): Form<AccountForm>) -> WebResult {
    check_origin(&headers)?;
    let next = safe_next(&f.next).to_string();
    let fail = |m: &str| Redirect::to(&format!("/-/login?next={}&err={}", enc(&next), enc(m))).into_response();
    let (token, max_age) = if !f.token.trim().is_empty() {
        match web.flux.principal(Some(f.token.trim())).await {
            Ok(p) if p.user.is_some() => (f.token.trim().to_string(), 30 * 86_400),
            _ => return Ok(fail("That token is invalid, expired or revoked")),
        }
    } else {
        match web.flux.password_login(f.login.trim(), &f.password, &client_of(&headers)).await {
            Ok((_, t)) => (t, crate::identity::SESSION_DAYS * 86_400),
            Err(e) => return Ok(fail(&e.to_string())),
        }
    };
    let mut r = Redirect::to(&next).into_response();
    r.headers_mut().insert(header::SET_COOKIE, session_cookie(&headers, &token, max_age)?);
    Ok(r)
}

pub async fn logout(State(web): State<WebState>, headers: HeaderMap) -> WebResult {
    check_origin(&headers)?;
    // End the session server-side too (API tokens used to sign in stay valid).
    if let Some(t) = super::cookie(&headers, COOKIE)
        && let Ok(p) = web.flux.principal(Some(t)).await
        && p.kind == "session"
    {
        let _ = web.flux.logout(t).await;
    }
    let mut r = Redirect::to("/").into_response();
    r.headers_mut()
        .insert(header::SET_COOKIE, HeaderValue::from_static("flux_token=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0"));
    Ok(r)
}

// ───────────── personal settings ─────────────

fn settings_layout(web: &WebState, req: &Req, title: &str, active: &str, content: Markup) -> Response {
    let is_admin = req.principal.user.as_ref().is_some_and(|u| u.is_admin);
    let item = |href: &str, key: &str, ic: &str, label: &str| {
        html! { a.side-link.active[active == key] href=(href) { (icon(ic)) (label) } }
    };
    let body = html! {
        div.container {
            div.settings-layout {
                nav.side-nav {
                    div.side-title { "Settings" }
                    (item("/-/settings", "profile", "user", "Profile"))
                    (item("/-/settings/tokens", "tokens", "key", "Access tokens"))
                    (item("/-/settings/orgs", "orgs", "org", "Organizations"))
                    @if is_admin {
                        div.side-title { "Site admin" }
                        (item("/-/admin", "users", "people", "Accounts"))
                        (item("/-/admin/audit", "audit", "history", "Audit log"))
                    }
                }
                div.settings-main { (notice(req)) (content) }
            }
        }
    };
    page(web, req, title, None, body)
}

pub async fn profile(State(web): State<WebState>, req: Req) -> WebResult {
    let u = me(&req)?;
    let acct = web.flux.meta.get_account_by_id(u.id).await?.ok_or_else(|| WebError::NotFound("account".into()))?;
    let content = html! {
        div.page-head { h2 { "Profile" } }
        div.card { form.form method="post" action="/-/settings/profile" {
            div.profile-row { (avatar(&u.name, acct.email.as_deref().unwrap_or(""), true)) div { div.h-name { (u.name) } div.muted.small { "Member since " (time(acct.created_at)) } } }
            label { "Display name" input type="text" name="display_name" value=(acct.display_name) maxlength="100"; }
            label { "Email" span.hint { "You can sign in with it." } input type="text" name="email" value=(acct.email.clone().unwrap_or_default()); }
            div.form-actions { button.btn.primary type="submit" { "Save profile" } }
        } }
        div.page-head { h2 { "Password" } }
        div.card { form.form method="post" action="/-/settings/password" {
            label { "Current password" span.hint { "Leave empty if your account has none yet." } input type="password" name="old_password" autocomplete="current-password"; }
            label { "New password" input type="password" name="password" required minlength="10" autocomplete="new-password"; }
            label { "Confirm new password" input type="password" name="password2" required minlength="10" autocomplete="new-password"; }
            div.form-actions { button.btn.primary type="submit" { "Change password" } }
        } }
    };
    Ok(settings_layout(&web, &req, "Profile", "profile", content))
}

pub async fn profile_post(
    State(web): State<WebState>,
    req: Req,
    headers: HeaderMap,
    Path(what): Path<String>,
    Form(f): Form<AccountForm>,
) -> WebResult {
    check_origin(&headers)?;
    let r = match what.as_str() {
        "profile" => web.flux.update_profile(&req.principal, &f.display_name, Some(&f.email)).await.map(|_| "Profile saved"),
        "password" if f.password != f.password2 => {
            return Ok(Redirect::to(&format!("/-/settings?err={}", enc("The passwords don't match"))).into_response());
        }
        "password" => web.flux.change_password(&req.principal, &f.old_password, &f.password).await.map(|_| "Password changed"),
        _ => return Err(WebError::NotFound("no such setting".into())),
    };
    Ok(match r {
        Ok(m) => back("/-/settings", m),
        Err(e) => Redirect::to(&format!("/-/settings?err={}", enc(&e.to_string()))).into_response(),
    })
}

pub async fn tokens(State(web): State<WebState>, req: Req) -> WebResult {
    me(&req)?;
    let list = web.flux.my_tokens(&req.principal).await?;
    // Shown once, only to the account that created it.
    let created = query(&req, "created")
        .and_then(|h| web.new_tokens.remove(&h))
        .filter(|(uid, _)| req.principal.user.as_ref().is_some_and(|u| u.id == *uid))
        .map(|(_, t)| t);
    let content = html! {
        div.page-head { h2 { "Access tokens" } }
        p.muted { "Tokens authenticate the " code { "flux" } " CLI, " code { "git" } " (as the password) and scripts. Scopes limit what a token can do, whatever your role." }
        @if let Some(t) = &created {
            div.notice.ok.token-once {
                div { strong { "Copy your new token now." } " It won't be shown again." }
                div.copy-row { code.token { (t) } button.btn.small.copy-btn type="button" data-copy=(t) { (icon("copy")) "Copy" } }
            }
        }
        div.card { form.form.row-form method="post" action="/-/settings/tokens" {
            label.grow { "Name" input type="text" name="label" placeholder="laptop, ci, deploy…" required maxlength="80"; }
            label { "Scopes" (role_select("scopes", "write", &["read", "write", "all"])) }
            label { "Expires" select name="expires" { option value="30" { "30 days" } option value="90" selected { "90 days" } option value="365" { "1 year" } option value="0" { "Never" } } }
            div.form-actions { button.btn.primary type="submit" { (icon("plus")) "Create token" } }
        } }
        div.card {
            div.card-head { "Active tokens and sessions" span.count { (list.len()) } }
            @if list.is_empty() { div.pad.muted { "No tokens yet." } }
            @for t in &list {
                div.list-row {
                    span.list-icon { (icon(if t.kind == "session" { "monitor" } else { "key" })) }
                    div.list-main {
                        div { strong { (t.label) } " " span.pill.small { (t.kind) } " " span.pill.small.accent { (t.scopes) } }
                        div.muted.small {
                            "Created " (time(t.created_at))
                            @if let Some(u) = t.last_used { " · last used " (time(u)) }
                            @match t.expires_at { Some(e) => { " · expires " (super::view::day_label(e)) } None => { " · never expires" } }
                        }
                    }
                    form method="post" action=(format!("/-/settings/tokens/{}/revoke", t.id)) {
                        button.btn.small.danger type="submit" { "Revoke" }
                    }
                }
            }
        }
    };
    Ok(settings_layout(&web, &req, "Access tokens", "tokens", content))
}

pub async fn token_create(State(web): State<WebState>, req: Req, headers: HeaderMap, Form(f): Form<AccountForm>) -> WebResult {
    check_origin(&headers)?;
    let days: i64 = f.expires.parse().unwrap_or(90);
    match web.flux.create_api_token(&req.principal, &f.label, &f.scopes, Some(days)).await {
        Ok(t) => {
            let mut h = [0u8; 16];
            getrandom::fill(&mut h).map_err(|e| WebError::Internal(e.to_string()))?;
            let handle = hex::encode(h);
            web.new_tokens.insert(handle.clone(), (me(&req)?.id, t));
            Ok(Redirect::to(&format!("/-/settings/tokens?created={handle}")).into_response())
        }
        Err(e) => Ok(Redirect::to(&format!("/-/settings/tokens?err={}", enc(&e.to_string()))).into_response()),
    }
}

pub async fn token_revoke(State(web): State<WebState>, req: Req, headers: HeaderMap, Path(id): Path<String>) -> WebResult {
    check_origin(&headers)?;
    web.flux.revoke_my_token(&req.principal, &id).await?;
    Ok(back("/-/settings/tokens", "Token revoked"))
}

// ───────────── organizations ─────────────

pub async fn my_orgs(State(web): State<WebState>, req: Req) -> WebResult {
    let u = me(&req)?;
    let orgs = web.flux.meta.orgs_of_user(u.id).await?;
    let content = html! {
        div.page-head { h2 { "Organizations" } }
        div.card {
            @if orgs.is_empty() { div.pad.muted { "You're not in any organization yet." } }
            @for (o, role) in &orgs {
                div.list-row {
                    (avatar(&o.name, "", false))
                    div.list-main { a href=(repo_url(&o.name)) { strong { (o.name) } } @if !o.display_name.is_empty() { " " span.muted { (o.display_name) } } }
                    span.pill.small { (role) }
                }
            }
        }
        div.page-head { h2 { "New organization" } }
        div.card { form.form method="post" action="/-/orgs" {
            label { "Name" span.hint { "Used in URLs: /name/repository." } input type="text" name="name" required maxlength="39" pattern="[A-Za-z0-9][A-Za-z0-9._\\-]*"; }
            label { "Display name" input type="text" name="display_name" maxlength="100"; }
            div.form-actions { button.btn.primary type="submit" { "Create organization" } }
        } }
    };
    Ok(settings_layout(&web, &req, "Organizations", "orgs", content))
}

pub async fn org_create(State(web): State<WebState>, req: Req, headers: HeaderMap, Form(f): Form<AccountForm>) -> WebResult {
    check_origin(&headers)?;
    match web.flux.create_org(&req.principal, f.name.trim(), &f.display_name).await {
        Ok(o) => Ok(Redirect::to(&repo_url(&o.name)).into_response()),
        Err(e) => Ok(Redirect::to(&format!("/-/settings/orgs?err={}", enc(&e.to_string()))).into_response()),
    }
}

struct OrgView {
    org: Org,
    my_role: Option<String>,
    owner: bool,
}

async fn org_view(web: &WebState, req: &Req, name: &str) -> WebResult<OrgView> {
    let org = web.flux.org_named(name).await?;
    let my_role = web.flux.org_role(&req.principal, &org).await?;
    let owner = web.flux.is_org_owner(&req.principal, &org).await?;
    Ok(OrgView { org, my_role, owner })
}

fn org_page(web: &WebState, req: &Req, v: &OrgView, tab: &str, content: Markup) -> Response {
    let o = &v.org;
    let t =
        |href: String, key: &str, ic: &str, label: &str| html! { a.tab.active[tab == key] href=(href) { (icon(ic)) (label) } };
    let body = html! {
        div.owner-head { div.container {
            div.owner-title {
                (avatar(&o.name, "", true))
                div {
                    h1 { (if o.display_name.is_empty() { &o.name } else { &o.display_name }) }
                    div.muted { (o.name) @if let Some(r) = &v.my_role { " · you're " (if r == "owner" { "an owner" } else { "a member" }) } }
                    @if !o.description.is_empty() { p.owner-desc { (o.description) } }
                }
            }
            nav.repo-tabs {
                (t(repo_url(&o.name), "repos", "repo", "Repositories"))
                (t(format!("/-/orgs/{}/people", enc(&o.name)), "people", "people", "People"))
                (t(format!("/-/orgs/{}/teams", enc(&o.name)), "teams", "team", "Teams"))
                @if v.owner { (t(format!("/-/orgs/{}/settings", enc(&o.name)), "settings", "settings", "Settings")) }
            }
        } }
        div.container { (notice(req)) (content) }
    };
    page(web, req, &o.name, None, body)
}

/// `/{name}` for a user or organization (when no repository has that name).
pub async fn owner_page(web: &WebState, req: &Req, name: &str) -> WebResult<Option<Response>> {
    let repos = web.flux.readable_repos(&req.principal).await?;
    let prefix = format!("{name}/");
    let mine: Vec<_> = repos.iter().filter(|r| r.name.starts_with(&prefix)).collect();
    let repo_list = |can_create: bool| {
        html! {
            div.page-head.row {
                h2 { "Repositories" }
                div.spacer {}
                @if can_create { a.btn.primary.small href=(format!("/-/new?owner={}", enc(name))) { (icon("plus")) "New repository" } }
            }
            @if mine.is_empty() { (empty_state("No repositories yet", html! { "Nothing here that you can see." })) }
            div.repo-list {
                @for r in &mine {
                    a.repo-row href=(repo_url(&r.name)) { (icon("repo")) span { (r.name.strip_prefix(&prefix).unwrap_or(&r.name)) } }
                }
            }
        }
    };
    if web.flux.meta.get_org(name).await?.is_some() {
        let v = org_view(web, req, name).await?;
        return Ok(Some(org_page(web, req, &v, "repos", repo_list(v.owner))));
    }
    let Some((acct, _)) = web.flux.meta.get_account(name).await?.filter(|(a, _)| a.user.name == name) else {
        return Ok(None);
    };
    let orgs = web.flux.meta.orgs_of_user(acct.user.id).await?;
    let is_me = req.principal.user.as_ref().is_some_and(|u| u.id == acct.user.id);
    let body = html! {
        div.container.profile {
            aside.profile-side {
                (avatar(&acct.user.name, acct.email.as_deref().unwrap_or(""), true))
                h1 { (if acct.display_name.is_empty() { &acct.user.name } else { &acct.display_name }) }
                div.muted { (acct.user.name) @if acct.user.is_admin { " · " span.pill.small.accent { "site admin" } } }
                div.muted.small { "Joined " (time(acct.created_at)) }
                @if !orgs.is_empty() {
                    div.side-title { "Organizations" }
                    div.org-chips { @for (o, _) in &orgs { a href=(repo_url(&o.name)) title=(o.name) { (avatar(&o.name, "", false)) } } }
                }
                @if is_me { a.btn.small href="/-/settings" { "Edit profile" } }
            }
            div.profile-main { (repo_list(is_me)) }
        }
    };
    Ok(Some(page(web, req, name, None, body)))
}

pub async fn org_people(State(web): State<WebState>, req: Req, Path(org): Path<String>) -> WebResult {
    let v = org_view(&web, &req, &org).await?;
    if v.my_role.is_none() && !v.owner {
        return Err(WebError::NotFound("organization members are visible to members".into()));
    }
    let members = web.flux.meta.org_members(v.org.id).await?;
    let action = format!("/-/orgs/{}/people", enc(&v.org.name));
    let content = html! {
        @if v.owner {
            div.card { form.form.row-form method="post" action=(action) {
                input type="hidden" name="action" value="add";
                label.grow { "Add a member" input type="text" name="user" placeholder="username" required; }
                label { "Role" (role_select("role", "member", &["member", "owner"])) }
                div.form-actions { button.btn.primary type="submit" { "Add" } }
            } }
        }
        div.card {
            div.card-head { "Members" span.count { (members.len()) } }
            @for (u, role) in &members {
                div.list-row {
                    (avatar(&u.name, "", false))
                    div.list-main { a href=(repo_url(&u.name)) { strong { (u.name) } } }
                    @if v.owner {
                        form.inline-form method="post" action=(action) {
                            input type="hidden" name="action" value="role";
                            input type="hidden" name="user" value=(u.name);
                            (role_select("role", role, &["member", "owner"]))
                            button.btn.small type="submit" { "Save" }
                        }
                        form method="post" action=(action) {
                            input type="hidden" name="action" value="remove";
                            input type="hidden" name="user" value=(u.name);
                            button.btn.small.danger type="submit" { "Remove" }
                        }
                    } @else {
                        span.pill.small { (role) }
                    }
                }
            }
        }
        @if v.my_role.is_some() && !v.owner {
            form method="post" action=(action) {
                input type="hidden" name="action" value="remove";
                input type="hidden" name="user" value=(req.principal.name());
                button.btn.small.danger type="submit" { "Leave " (v.org.name) }
            }
        }
    };
    Ok(org_page(&web, &req, &v, "people", content))
}

pub async fn org_people_post(
    State(web): State<WebState>,
    req: Req,
    headers: HeaderMap,
    Path(org): Path<String>,
    Form(f): Form<AccountForm>,
) -> WebResult {
    check_origin(&headers)?;
    let o = web.flux.org_named(&org).await?;
    let to = format!("/-/orgs/{}/people", enc(&o.name));
    let r = match f.action.as_str() {
        "add" | "role" => {
            web.flux.set_org_member(&req.principal, &o, f.user.trim(), Some(f.role.as_str())).await.map(|_| "Member saved")
        }
        "remove" => web.flux.set_org_member(&req.principal, &o, f.user.trim(), None).await.map(|_| "Member removed"),
        _ => return Err(WebError::BadRequest("unknown action".into())),
    };
    Ok(match r {
        Ok(m) => back(&to, m),
        Err(e) => Redirect::to(&format!("{to}?err={}", enc(&e.to_string()))).into_response(),
    })
}

pub async fn org_teams(State(web): State<WebState>, req: Req, Path(org): Path<String>) -> WebResult {
    let v = org_view(&web, &req, &org).await?;
    if v.my_role.is_none() && !v.owner {
        return Err(WebError::NotFound("teams are visible to members".into()));
    }
    let teams = web.flux.meta.list_teams(v.org.id).await?;
    let mut sizes = Vec::new();
    for t in &teams {
        sizes.push(web.flux.meta.team_members(t.id).await?.len());
    }
    let content = html! {
        @if v.owner {
            div.card { form.form.row-form method="post" action=(format!("/-/orgs/{}/teams", enc(&v.org.name))) {
                label.grow { "New team" input type="text" name="name" placeholder="backend" required maxlength="39"; }
                label.grow { "Description" input type="text" name="description" maxlength="200"; }
                div.form-actions { button.btn.primary type="submit" { "Create team" } }
            } }
        }
        div.card {
            div.card-head { "Teams" span.count { (teams.len()) } }
            @if teams.is_empty() { div.pad.muted { "No teams yet. Teams grant roles on repositories to groups of members." } }
            @for (t, n) in teams.iter().zip(&sizes) {
                a.list-row.link href=(format!("/-/orgs/{}/teams/{}", enc(&v.org.name), enc(&t.name))) {
                    span.list-icon { (icon("team")) }
                    div.list-main { strong { (t.name) } @if !t.description.is_empty() { div.muted.small { (t.description) } } }
                    span.muted.small { (n) " member" @if *n != 1 { "s" } }
                }
            }
        }
    };
    Ok(org_page(&web, &req, &v, "teams", content))
}

pub async fn org_team_create(
    State(web): State<WebState>,
    req: Req,
    headers: HeaderMap,
    Path(org): Path<String>,
    Form(f): Form<AccountForm>,
) -> WebResult {
    check_origin(&headers)?;
    let o = web.flux.org_named(&org).await?;
    let to = format!("/-/orgs/{}/teams", enc(&o.name));
    Ok(match web.flux.create_team(&req.principal, &o, f.name.trim(), &f.description).await {
        Ok(t) => Redirect::to(&format!("{to}/{}", enc(&t.name))).into_response(),
        Err(e) => Redirect::to(&format!("{to}?err={}", enc(&e.to_string()))).into_response(),
    })
}

pub async fn org_team(State(web): State<WebState>, req: Req, Path((org, team)): Path<(String, String)>) -> WebResult {
    let v = org_view(&web, &req, &org).await?;
    if v.my_role.is_none() && !v.owner {
        return Err(WebError::NotFound("teams are visible to members".into()));
    }
    let (_, t) = web.flux.team_named(&org, &team).await?;
    let members = web.flux.meta.team_members(t.id).await?;
    let mut repos = Vec::new();
    let all = web.flux.meta.list_repos().await?;
    for (id, role) in web.flux.meta.team_repos(t.id).await? {
        if let Some(r) = all.iter().find(|r| r.id == id) {
            repos.push((r.name.clone(), role));
        }
    }
    let action = format!("/-/orgs/{}/teams/{}", enc(&v.org.name), enc(&t.name));
    let content = html! {
        div.page-head.row {
            div { h2 { (icon("team")) " " (t.name) } @if !t.description.is_empty() { p.muted { (t.description) } } }
            div.spacer {}
            @if v.owner {
                form method="post" action=(action) {
                    input type="hidden" name="action" value="delete";
                    button.btn.small.danger type="submit" { "Delete team" }
                }
            }
        }
        div.two-col {
            div.card {
                div.card-head { "Members" span.count { (members.len()) } }
                @if v.owner {
                    form.form.row-form method="post" action=(action) {
                        input type="hidden" name="action" value="add";
                        label.grow { input type="text" name="user" placeholder="Add an org member" required; }
                        button.btn.small type="submit" { "Add" }
                    }
                }
                @for u in &members {
                    div.list-row {
                        (avatar(&u.name, "", false))
                        div.list-main { a href=(repo_url(&u.name)) { (u.name) } }
                        @if v.owner {
                            form method="post" action=(action) {
                                input type="hidden" name="action" value="remove";
                                input type="hidden" name="user" value=(u.name);
                                button.btn.small type="submit" { "Remove" }
                            }
                        }
                    }
                }
            }
            div.card {
                div.card-head { "Repository access" span.count { (repos.len()) } }
                @if repos.is_empty() { div.pad.muted.small { "Grant this team a role from a repository's Settings → Access page." } }
                @for (name, role) in &repos {
                    div.list-row { (icon("repo")) div.list-main { a href=(repo_url(name)) { (name) } } span.pill.small.accent { (role) } }
                }
            }
        }
    };
    Ok(org_page(&web, &req, &v, "teams", content))
}

pub async fn org_team_post(
    State(web): State<WebState>,
    req: Req,
    headers: HeaderMap,
    Path((org, team)): Path<(String, String)>,
    Form(f): Form<AccountForm>,
) -> WebResult {
    check_origin(&headers)?;
    let (o, t) = web.flux.team_named(&org, &team).await?;
    let to = format!("/-/orgs/{}/teams/{}", enc(&o.name), enc(&t.name));
    let r = match f.action.as_str() {
        "add" => web.flux.set_team_member(&req.principal, &o, &t, f.user.trim(), true).await.map(|_| "Member added"),
        "remove" => web.flux.set_team_member(&req.principal, &o, &t, f.user.trim(), false).await.map(|_| "Member removed"),
        "delete" => {
            web.flux.delete_team(&req.principal, &o, &t).await?;
            return Ok(back(&format!("/-/orgs/{}/teams", enc(&o.name)), "Team deleted"));
        }
        _ => return Err(WebError::BadRequest("unknown action".into())),
    };
    Ok(match r {
        Ok(m) => back(&to, m),
        Err(e) => Redirect::to(&format!("{to}?err={}", enc(&e.to_string()))).into_response(),
    })
}

pub async fn org_settings(State(web): State<WebState>, req: Req, Path(org): Path<String>) -> WebResult {
    let v = org_view(&web, &req, &org).await?;
    if !v.owner {
        return Err(WebError::Forbidden("only owners can change organization settings".into()));
    }
    let o = &v.org;
    let content = html! {
        div.card { form.form method="post" action=(format!("/-/orgs/{}/settings", enc(&o.name))) {
            label { "Display name" input type="text" name="display_name" value=(o.display_name) maxlength="100"; }
            label { "Description" input type="text" name="description" value=(o.description) maxlength="300"; }
            label { "Base role for members"
                span.hint { "What every member can do on all of this organization's repositories. Teams and collaborators can grant more." }
                (role_select("base_role", &o.base_role, &["none", "read", "write"]))
            }
            div.form-actions { button.btn.primary type="submit" { "Save settings" } }
        } }
    };
    Ok(org_page(&web, &req, &v, "settings", content))
}

pub async fn org_settings_post(
    State(web): State<WebState>,
    req: Req,
    headers: HeaderMap,
    Path(org): Path<String>,
    Form(f): Form<AccountForm>,
) -> WebResult {
    check_origin(&headers)?;
    let o = web.flux.org_named(&org).await?;
    let to = format!("/-/orgs/{}/settings", enc(&o.name));
    Ok(match web.flux.update_org(&req.principal, &o, &f.display_name, &f.description, &f.base_role).await {
        Ok(()) => back(&to, "Settings saved"),
        Err(e) => Redirect::to(&format!("{to}?err={}", enc(&e.to_string()))).into_response(),
    })
}

// ───────────── repositories ─────────────

pub async fn new_repo(State(web): State<WebState>, req: Req) -> WebResult {
    let u = me(&req)?;
    let orgs = web.flux.meta.orgs_of_user(u.id).await?;
    let owners: Vec<String> =
        std::iter::once(u.name.clone()).chain(orgs.iter().filter(|(_, r)| r == "owner").map(|(o, _)| o.name.clone())).collect();
    let selected = query(&req, "owner").unwrap_or_else(|| u.name.clone());
    let vis = |v: &str, title: &str, text: &str, checked: bool| {
        html! {
            label.choice {
                input type="radio" name="visibility" value=(v) checked[checked];
                span { strong { (title) } span.muted.small { (text) } }
            }
        }
    };
    let body = html! {
        div.container.narrow {
            div.page-head { h1 { "New repository" } }
            (notice(&req))
            div.card { form.form method="post" action="/-/repos" {
                div.owner-name {
                    label { "Owner" select name="owner" {
                        @for o in &owners { option value=(o) selected[*o == selected] { (o) } }
                        @if u.is_admin { option value="" { "(server-wide)" } }
                    } }
                    span.slash { "/" }
                    label.grow { "Name" input type="text" name="name" required autofocus maxlength="100" pattern="[A-Za-z0-9][A-Za-z0-9._\\-]*"; }
                }
                div.choices {
                    (vis("private", "Private", "Only people and teams you grant access.", true))
                    (vis("internal", "Internal", "Everyone signed in to this server can read.", false))
                    (vis("public", "Public", "Anyone who can reach this server can read.", false))
                }
                div.form-actions { button.btn.primary type="submit" { "Create repository" } }
            } }
        }
    };
    Ok(page(&web, &req, "New repository", None, body))
}

pub async fn create_repo(State(web): State<WebState>, req: Req, headers: HeaderMap, Form(f): Form<AccountForm>) -> WebResult {
    check_origin(&headers)?;
    let name =
        if f.owner.trim().is_empty() { f.name.trim().to_string() } else { format!("{}/{}", f.owner.trim(), f.name.trim()) };
    let vis = Some(f.visibility.trim()).filter(|v| !v.is_empty());
    Ok(match web.flux.create_repo_as(&req.principal, &name, vis).await {
        Ok(_) => Redirect::to(&repo_url(&name)).into_response(),
        Err(e) => Redirect::to(&format!("/-/new?owner={}&err={}", enc(f.owner.trim()), enc(&e.to_string()))).into_response(),
    })
}

/// Repository Settings → Access.
pub async fn repo_access(web: &WebState, repo: RepoId, name: &str) -> WebResult<Markup> {
    let access = web.flux.repo_access(repo).await?;
    let grants = web.flux.meta.repo_grants(repo).await?;
    let owner = match access.owner_kind.as_str() {
        "org" => web.flux.meta.get_org_by_id(access.owner_id).await?.map(|o| format!("the {} organization", o.name)),
        "user" => web.flux.meta.get_account_by_id(access.owner_id).await?.map(|a| a.user.name),
        _ => None,
    };
    let action = format!("{}/-/settings/access", repo_url(name));
    let roles: Vec<&str> = ROLES.iter().map(|r| r.as_str()).collect();
    Ok(html! {
        div.card {
            div.card-head { (icon("lock")) "Visibility" }
            form.form method="post" action=(action) {
                input type="hidden" name="action" value="visibility";
                p.muted.small {
                    @match &owner { Some(o) => { "Owned by " (o) ". " } None => { "A server-wide repository: any signed-in user may write. " } }
                    "Owners and site admins always have full access."
                }
                div.choices {
                    @for (v, t, d) in [("private", "Private", "Only people and teams granted access."), ("internal", "Internal", "Everyone signed in can read."), ("public", "Public", "Anyone who can reach the server can read.")] {
                        label.choice { input type="radio" name="visibility" value=(v) checked[access.visibility == v]; span { strong { (t) } span.muted.small { (d) } } }
                    }
                }
                div.form-actions { button.btn.primary type="submit" { "Save visibility" } }
            }
        }
        div.card {
            div.card-head { (icon("people")) "People and teams" span.count { (grants.len()) } }
            form.form.row-form method="post" action=(action) {
                input type="hidden" name="action" value="grant";
                label { "Grant to" select name="kind" { option value="user" { "user" } option value="team" { "team" } } }
                label.grow { "Name" input type="text" name="subject" placeholder="username, or team (org/team)" required; }
                label { "Role" (role_select("role", "write", &roles)) }
                div.form-actions { button.btn.primary type="submit" { "Grant" } }
            }
            @for g in &grants {
                div.list-row {
                    span.list-icon { (icon(if g.kind == "team" { "team" } else { "user" })) }
                    div.list-main { strong { (g.subject_name) } " " span.muted.small { (g.kind) } }
                    form.inline-form method="post" action=(action) {
                        input type="hidden" name="action" value="grant";
                        input type="hidden" name="kind" value=(g.kind);
                        input type="hidden" name="subject" value=(g.subject_name);
                        (role_select("role", &g.role, &roles))
                        button.btn.small type="submit" { "Save" }
                    }
                    form method="post" action=(action) {
                        input type="hidden" name="action" value="revoke";
                        input type="hidden" name="kind" value=(g.kind);
                        input type="hidden" name="subject" value=(g.subject_name);
                        button.btn.small.danger type="submit" { "Remove" }
                    }
                }
            }
        }
        div.card {
            div.card-head { (icon("shield")) "Roles" }
            div.roles-table { @for r in ROLES { div { span.pill.small.accent { (r.as_str()) } } div.muted.small { (r.describe()) } } }
        }
    })
}

pub async fn repo_access_post(web: &WebState, req: &Req, repo: RepoId, name: &str, f: &AccountForm) -> WebResult<Response> {
    let to = format!("{}/-/settings/access", repo_url(name));
    let r = match f.action.as_str() {
        "visibility" => {
            web.flux.set_visibility(&req.principal, repo, name, f.visibility.trim()).await.map(|_| "Visibility saved")
        }
        "grant" => {
            let role = Role::parse(f.role.trim()).ok_or_else(|| WebError::BadRequest("unknown role".into()))?;
            web.flux
                .set_grant(&req.principal, repo, name, f.kind.trim(), f.subject.trim(), Some(role))
                .await
                .map(|_| "Access saved")
        }
        "revoke" => {
            web.flux.set_grant(&req.principal, repo, name, f.kind.trim(), f.subject.trim(), None).await.map(|_| "Access removed")
        }
        _ => return Err(WebError::BadRequest("unknown action".into())),
    };
    Ok(match r {
        Ok(m) => back(&to, m),
        Err(e) => Redirect::to(&format!("{to}?err={}", enc(&e.to_string()))).into_response(),
    })
}

// ───────────── site admin ─────────────

fn require_site_admin(p: &Principal) -> WebResult<()> {
    if p.user.as_ref().is_some_and(|u| u.is_admin) || p.mode == crate::auth::AuthMode::Open {
        Ok(())
    } else {
        Err(WebError::Forbidden("site admins only".into()))
    }
}

pub async fn admin_users(State(web): State<WebState>, req: Req) -> WebResult {
    require_site_admin(&req.principal)?;
    let accounts = web.flux.meta.list_accounts().await?;
    let orgs = web.flux.meta.list_orgs().await?;
    let repos = web.flux.meta.list_repos().await?;
    let content = html! {
        div.stat-row {
            div.stat { div.stat-n { (accounts.len()) } div.muted.small { "accounts" } }
            div.stat { div.stat-n { (orgs.len()) } div.muted.small { "organizations" } }
            div.stat { div.stat-n { (repos.len()) } div.muted.small { "repositories" } }
            div.stat { div.stat-n { (if web.flux.signup == crate::identity::SignupPolicy::Open { "open" } else { "closed" }) } div.muted.small { "sign-up" } }
        }
        div.card { form.form.row-form method="post" action="/-/admin/users" {
            input type="hidden" name="action" value="create";
            label.grow { "New account" input type="text" name="name" placeholder="username" required maxlength="39"; }
            label.grow { "Email" input type="text" name="email"; }
            label.grow { "Initial password" input type="password" name="password" minlength="10" autocomplete="new-password"; }
            label { "Role" (role_select("role", "user", &["user", "admin"])) }
            div.form-actions { button.btn.primary type="submit" { "Create" } }
        } }
        div.card {
            div.card-head { "Accounts" span.count { (accounts.len()) } }
            @for a in &accounts {
                div.list-row.dim[a.disabled] {
                    (avatar(&a.user.name, a.email.as_deref().unwrap_or(""), false))
                    div.list-main {
                        a href=(repo_url(&a.user.name)) { strong { (a.user.name) } }
                        @if a.user.is_admin { " " span.pill.small.accent { "admin" } }
                        @if a.disabled { " " span.pill.small { "disabled" } }
                        div.muted.small { (a.email.clone().unwrap_or_default()) " · joined " (time(a.created_at)) }
                    }
                    form method="post" action=(format!("/-/admin/users/{}", enc(&a.user.name))) {
                        input type="hidden" name="action" value=(if a.user.is_admin { "demote" } else { "promote" });
                        button.btn.small type="submit" { (if a.user.is_admin { "Remove admin" } else { "Make admin" }) }
                    }
                    form method="post" action=(format!("/-/admin/users/{}", enc(&a.user.name))) {
                        input type="hidden" name="action" value=(if a.disabled { "enable" } else { "disable" });
                        button.btn.small.danger[!a.disabled] type="submit" { (if a.disabled { "Enable" } else { "Disable" }) }
                    }
                }
            }
        }
    };
    Ok(settings_layout(&web, &req, "Accounts", "users", content))
}

pub async fn admin_users_post(
    State(web): State<WebState>,
    req: Req,
    headers: HeaderMap,
    Form(f): Form<AccountForm>,
) -> WebResult {
    check_origin(&headers)?;
    let email = Some(f.email.trim()).filter(|e| !e.is_empty());
    let password = Some(f.password.as_str()).filter(|p| !p.is_empty());
    Ok(match web.flux.admin_create_account(&req.principal, f.name.trim(), email, password, f.role == "admin").await {
        Ok(_) => back("/-/admin", "Account created"),
        Err(e) => Redirect::to(&format!("/-/admin?err={}", enc(&e.to_string()))).into_response(),
    })
}

pub async fn admin_user_post(
    State(web): State<WebState>,
    req: Req,
    headers: HeaderMap,
    Path(name): Path<String>,
    Form(f): Form<AccountForm>,
) -> WebResult {
    check_origin(&headers)?;
    let (admin, disabled) = match f.action.as_str() {
        "promote" => (Some(true), None),
        "demote" => (Some(false), None),
        "disable" => (None, Some(true)),
        "enable" => (None, Some(false)),
        _ => return Err(WebError::BadRequest("unknown action".into())),
    };
    Ok(match web.flux.admin_update_account(&req.principal, &name, admin, disabled).await {
        Ok(()) => back("/-/admin", "Account updated"),
        Err(e) => Redirect::to(&format!("/-/admin?err={}", enc(&e.to_string()))).into_response(),
    })
}

pub async fn admin_audit(State(web): State<WebState>, req: Req) -> WebResult {
    require_site_admin(&req.principal)?;
    let before = query(&req, "before").and_then(|b| b.parse().ok());
    let entries = web.flux.meta.audit_list(before, 100).await?;
    let content = html! {
        div.page-head { h2 { "Audit log" } }
        div.card {
            @if entries.is_empty() { div.pad.muted { "Nothing recorded yet." } }
            @for e in &entries {
                div.list-row.audit {
                    span.audit-action.pill.small.danger[e.action.ends_with("failed")] { (e.action) }
                    div.list-main { strong { (e.actor) } " " span { (e.target) } @if !e.detail.is_empty() { " " span.muted.small { (e.detail) } } }
                    span.muted.small { (time(e.ts)) }
                }
            }
        }
        @if let Some(last) = entries.last().filter(|_| entries.len() == 100) {
            a.btn.small href=(format!("/-/admin/audit?before={}", last.seq)) { "Older" }
        }
    };
    Ok(settings_layout(&web, &req, "Audit log", "audit", content))
}
