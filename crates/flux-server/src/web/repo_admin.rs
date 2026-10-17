//! Repository administration: webhooks, branch protection, secrets, the
//! danger zone (rename, archive, delete), forks and watching.

use maud::{Markup, html};

use super::forms::{Fields, done, to};
use super::pages::{RepoCtx, nav, parse_num, settings_tabs};
use super::view::{Tab, enc, icon, page, repo_url, sdot, time};
use super::{Req, WebError, WebResult, WebState};
use crate::hooks::{EVENTS, HookInput, Scope};
use crate::protect::BranchRule;
use crate::rbac::Role;

fn settings_base(ctx: &RepoCtx) -> String {
    format!("/{}/-/settings", enc(&ctx.name))
}

pub async fn settings_get(web: &WebState, req: &Req, ctx: &RepoCtx, args: &[String]) -> WebResult {
    ctx.require(Role::Admin)?;
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    let (tab, title, sub, content) = match a.as_slice() {
        ["hooks"] => (
            "hooks",
            "Webhooks",
            html! { "Send events from " (ctx.name) " to other services." },
            hooks_list(web, req, ctx).await?,
        ),
        ["hooks", id] => (
            "hooks",
            "Webhook",
            html! { "Send events from " (ctx.name) " to other services." },
            hook_detail(web, req, ctx, parse_num(id)?).await?,
        ),
        ["branches"] => (
            "branches",
            "Branch protection",
            html! { "Rules for how branches of " (ctx.name) " may change." },
            branches(web, ctx).await?,
        ),
        ["secrets"] => (
            "secrets",
            "Secrets",
            html! { "Encrypted values available to CI jobs as " code { "${{ secrets.NAME }}" } "." },
            secrets(web, req, ctx).await?,
        ),
        _ => return Err(WebError::NotFound("no such settings page".into())),
    };
    let body = html! {
        div.container.narrow {
            div.page-head { h1 { "Settings" } p.muted { (sub) } }
            (settings_tabs(ctx, tab))
            (content)
        }
    };
    let rev = ctx.default_branch().unwrap_or("main").to_string();
    let n = nav(web, req, ctx, Tab::Settings, &rev).await;
    Ok(page(web, req, &format!("{title} · {}", ctx.name), Some(n), body))
}

// ───────────── webhooks ─────────────

fn event_checks(selected: &[String]) -> Markup {
    let all = selected.iter().any(|e| e == "*");
    html! {
        fieldset.events {
            legend { "Events" }
            label.check.strong { input type="checkbox" name="events" value="*" checked[all]; " Everything" }
            div.event-grid {
                @for e in EVENTS.iter().filter(|e| **e != "ping") {
                    label.check { input type="checkbox" name="events" value=(e) checked[!all && selected.iter().any(|s| s == e)]; code { (e) } }
                }
            }
        }
    }
}

async fn hooks_list(web: &WebState, req: &Req, ctx: &RepoCtx) -> WebResult<Markup> {
    let hooks = web.flux.list_hooks(&req.principal, Scope::Repo(ctx.id)).await?;
    let b = format!("{}/hooks", settings_base(ctx));
    Ok(html! {
        div.card {
            div.card-head { (icon("webhook")) "Webhooks" span.count { (hooks.len()) } }
            @if hooks.is_empty() { div.pad.muted { "No webhooks yet. Payloads are GitHub-compatible, so most receivers work unchanged." } }
            @for h in &hooks {
                a.list-row.link href=(format!("{b}/{}", h.id)) {
                    span.list-icon { (icon("webhook")) }
                    div.list-main {
                        strong.mono.small { (h.url) }
                        div.muted.small { (h.events.join(", ")) " · added " (time(h.created_at)) }
                    }
                    @if h.active { span.pill.small.ok { "Active" } } @else { span.pill.small { "Inactive" } }
                }
            }
        }
        div.card {
            div.card-head { (icon("plus")) "Add a webhook" }
            form.form method="post" action=(b) {
                label { "Payload URL" input.mono type="text" name="url" required placeholder="https://example.com/hooks/flux" inputmode="url"; }
                label { "Secret" span.hint { "Signs each delivery (" code { "X-Hub-Signature-256" } "). Optional." } input type="password" name="secret" autocomplete="new-password"; }
                (event_checks(&["push".to_string()]))
                label.check { input type="checkbox" name="active" value="1" checked; " Active" }
                div.form-actions { button.btn.primary type="submit" { "Add webhook" } }
            }
        }
    })
}

async fn hook_detail(web: &WebState, req: &Req, ctx: &RepoCtx, id: i64) -> WebResult<Markup> {
    let scope = Scope::Repo(ctx.id);
    let h = web.flux.hook_in(&req.principal, scope, id).await?;
    let deliveries = web.flux.hook_deliveries(&req.principal, scope, id).await?;
    let b = format!("{}/hooks/{id}", settings_base(ctx));
    Ok(html! {
        p { a.link.small href=(format!("{}/hooks", settings_base(ctx))) { "← All webhooks" } }
        div.card {
            div.card-head { (icon("webhook")) span.mono.small { (h.url) } div.spacer {}
                form method="post" action=(format!("{b}/ping")) { button.btn.small type="submit" { (icon("refresh")) "Ping" } }
            }
            form.form method="post" action=(b) {
                label { "Payload URL" input.mono type="text" name="url" value=(h.url) required; }
                label { "Secret"
                    span.hint { @if h.secret.is_empty() { "No secret set." } @else { "A secret is set. Leave empty to keep it." } }
                    input type="password" name="secret" autocomplete="new-password" placeholder=(if h.secret.is_empty() { "" } else { "••••••••" });
                }
                @if !h.secret.is_empty() { label.check { input type="checkbox" name="clear_secret" value="1"; " Remove the secret" } }
                (event_checks(&h.events))
                label.check { input type="checkbox" name="active" value="1" checked[h.active]; " Active" }
                div.form-actions { button.btn.primary type="submit" { "Update webhook" } }
            }
        }
        div.card {
            div.card-head { (icon("history")) "Recent deliveries" span.count { (deliveries.len()) } }
            @if deliveries.is_empty() { div.pad.muted { "Nothing delivered yet." } }
            @for d in &deliveries {
                details.delivery {
                    summary {
                        (sdot(match d.state.as_str() { "ok" => "success", "failed" => "failure", _ => "pending" }))
                        code.small { (&d.guid[..8.min(d.guid.len())]) }
                        span.pill.small { (d.event) }
                        div.spacer {}
                        @if d.status_code > 0 { span.mono.small class=(if (200..300).contains(&d.status_code) { "ok-text" } else { "bad-text" }) { (d.status_code) } }
                        @else { span.muted.small { (d.state) } }
                        span.muted.small { (d.duration_ms) " ms" }
                        span.muted.small.hide-sm { (time(d.created_at)) }
                    }
                    div.delivery-body {
                        div.row.small.muted {
                            span { "Delivery " code { (d.guid) } }
                            span { " · " (d.attempts) " attempt" @if d.attempts != 1 { "s" } }
                            @if let Some(n) = d.next_attempt_at { span { " · next attempt " (time(n)) } }
                            div.spacer {}
                            form method="post" action=(format!("{b}/deliveries/{}/redeliver", d.id)) {
                                button.btn.small type="submit" { (icon("refresh")) "Redeliver" }
                            }
                        }
                        h4 { "Request payload" }
                        pre.payload { (serde_json::to_string_pretty(&d.payload).unwrap_or_default()) }
                        h4 { "Response" }
                        pre.payload { @if d.response.is_empty() { span.muted { "(empty)" } } @else { (d.response) } }
                    }
                }
            }
        }
        div.card.danger-zone {
            div.list-row {
                div.list-main { strong { "Delete this webhook" } div.muted.small { "Deliveries stop immediately." } }
                form method="post" action=(format!("{b}/delete")) data-confirm="Delete this webhook?" {
                    button.btn.small.danger type="submit" { (icon("trash")) "Delete" }
                }
            }
        }
    })
}

fn hook_input(f: &Fields, create: bool) -> HookInput {
    let mut events = f.all("events");
    if events.iter().any(|e| e == "*") {
        events = vec!["*".into()];
    }
    let secret = if f.checked("clear_secret") {
        Some(String::new())
    } else if create || !f.get("secret").is_empty() {
        Some(f.get("secret").to_string())
    } else {
        None
    };
    HookInput {
        url: f.trim("url").to_string(),
        secret,
        events: (!events.is_empty()).then_some(events),
        active: Some(f.checked("active")),
    }
}

// ───────────── branch protection ─────────────

fn rule_fields(r: &BranchRule) -> Markup {
    html! {
        label { "Branch name pattern" span.hint { code { "main" } ", " code { "release/*" } " (one segment) or " code { "release/**" } " (any depth)." }
            input.mono type="text" name="pattern" value=(r.pattern) required placeholder="main";
        }
        div.rule-grid {
            label.check { input type="checkbox" name="require_review" value="1" checked[r.require_review]; span { strong { "Require a review" } span.hint { "No direct pushes; changes land through reviews." } } }
            label.check { input type="checkbox" name="require_codeowners" value="1" checked[r.require_codeowners]; span { strong { "Require code owners" } span.hint { "Files with owners need one of their approvals." } } }
            label.check { input type="checkbox" name="dismiss_stale_approvals" value="1" checked[r.dismiss_stale_approvals]; span { strong { "Dismiss stale approvals" } span.hint { "New pushes reset approvals." } } }
            label.check { input type="checkbox" name="enforce_admins" value="1" checked[r.enforce_admins]; span { strong { "Include administrators" } span.hint { "Admins can't bypass this rule." } } }
            label.check { input type="checkbox" name="allow_force_push" value="1" checked[r.allow_force_push]; span { strong { "Allow force pushes" } span.hint { "Permit non-fast-forward updates." } } }
            label.check { input type="checkbox" name="allow_deletion" value="1" checked[r.allow_deletion]; span { strong { "Allow deletion" } span.hint { "Permit deleting matching branches." } } }
        }
        div.row2 {
            label { "Required approvals" input type="number" name="required_approvals" min="0" max="10" value=(r.required_approvals); }
            label { "Required status checks" span.hint { "Contexts, comma separated." } input.mono type="text" name="required_checks" value=(r.required_checks.join(", ")) placeholder="flux-ci/CI/test"; }
        }
        label { "Push allow list" span.hint { "Users or " code { "org/team" } "s who may push directly despite requiring a review." }
            input type="text" name="push_allow" value=(r.push_allow.join(", ")) placeholder="release-bot, acme/release";
        }
    }
}

fn rule_from(f: &Fields) -> WebResult<BranchRule> {
    let r = BranchRule {
        pattern: f.trim("pattern").to_string(),
        require_review: f.checked("require_review"),
        required_approvals: f.trim("required_approvals").parse().unwrap_or(0),
        require_codeowners: f.checked("require_codeowners"),
        dismiss_stale_approvals: f.checked("dismiss_stale_approvals"),
        required_checks: f
            .get("required_checks")
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect(),
        allow_force_push: f.checked("allow_force_push"),
        allow_deletion: f.checked("allow_deletion"),
        push_allow: f.list("push_allow"),
        enforce_admins: f.checked("enforce_admins"),
    };
    if r.pattern.is_empty() {
        return Err(WebError::BadRequest("every rule needs a branch name pattern".into()));
    }
    if r.required_approvals > 10 {
        return Err(WebError::BadRequest("at most 10 required approvals".into()));
    }
    Ok(r)
}

async fn branches(web: &WebState, ctx: &RepoCtx) -> WebResult<Markup> {
    let cfg = web.flux.repo_config(ctx.id).await?;
    let defaults = cfg.branch_rules.is_none();
    let rules = cfg.branch_rules.clone().unwrap_or_else(crate::protect::default_rules);
    let b = format!("{}/branches", settings_base(ctx));
    Ok(html! {
        @if defaults {
            div.notice.rounded {
                (icon("shield")) " Using the built-in defaults: " code { "main" } ", " code { "master" } ", " code { "trunk" } " and " code { "release/*" }
                " move only forward and can't be deleted, except by admins. Saving a rule replaces the defaults."
            }
        }
        @for (i, r) in rules.iter().enumerate() {
            details.card.rule open[rules.len() == 1] {
                summary {
                    (icon("branch")) code { (r.pattern) }
                    div.rule-tags {
                        @if r.require_review { span.pill.small { "review" } }
                        @if r.required_approvals > 0 { span.pill.small { (r.required_approvals) " approval" @if r.required_approvals != 1 { "s" } } }
                        @if !r.required_checks.is_empty() { span.pill.small { (r.required_checks.len()) " check" @if r.required_checks.len() != 1 { "s" } } }
                        @if r.require_codeowners { span.pill.small { "code owners" } }
                        @if r.allow_force_push { span.pill.small.warn { "force push" } }
                    }
                }
                form.form method="post" action=(b) {
                    input type="hidden" name="index" value=(i);
                    (rule_fields(r))
                    div.form-actions {
                        button.btn.danger type="submit" name="action" value="delete" formnovalidate { (icon("trash")) "Delete rule" }
                        button.btn.primary type="submit" name="action" value="save" { "Save rule" }
                    }
                }
            }
        }
        details.card.rule open[rules.is_empty()] {
            summary { (icon("plus")) "Add a rule" }
            form.form method="post" action=(b) {
                input type="hidden" name="action" value="add";
                (rule_fields(&BranchRule::default()))
                div.form-actions { button.btn.primary type="submit" { "Add rule" } }
            }
        }
        @if !defaults {
            form method="post" action=(b) data-confirm="Replace these rules with the built-in defaults?" {
                input type="hidden" name="action" value="reset";
                button.btn.small type="submit" { (icon("refresh")) "Restore defaults" }
            }
        }
    })
}

// ───────────── secrets ─────────────

async fn secrets(web: &WebState, req: &Req, ctx: &RepoCtx) -> WebResult<Markup> {
    let no_key = web.flux.settings.secret_key.is_none();
    let list = if no_key { vec![] } else { web.flux.list_secrets(&req.principal, ("repo", ctx.id.0)).await? };
    let b = format!("{}/secrets", settings_base(ctx));
    Ok(html! {
        @if no_key {
            div.notice.error { (icon("alert")) " Secrets need a server secret key. Start the server with " code { "--secret-key-file" } "." }
        }
        div.card {
            div.card-head { (icon("key")) "Repository secrets" span.count { (list.len()) } }
            @if list.is_empty() && !no_key { div.pad.muted { "No secrets yet. Values are encrypted at rest and never shown again." } }
            @for s in &list {
                div.list-row {
                    span.list-icon { (icon("lock")) }
                    div.list-main { strong.mono { (s.name) } div.muted.small { "Updated " (time(s.updated_at)) } }
                    details.menu.row-menu {
                        summary.btn.small { "Update" }
                        div.menu-pop.wide {
                            form.form-tight method="post" action=(b) {
                                input type="hidden" name="name" value=(s.name);
                                label { "New value for " code { (s.name) } textarea.mono name="value" rows="3" required autocomplete="off" spellcheck="false" {} }
                                div.form-actions { button.btn.small.primary type="submit" { "Update secret" } }
                            }
                        }
                    }
                    form method="post" action=(format!("{b}/delete")) data-confirm=(format!("Delete the secret {}?", s.name)) {
                        input type="hidden" name="name" value=(s.name);
                        button.btn.small.danger type="submit" aria-label=(format!("Delete {}", s.name)) { (icon("trash")) }
                    }
                }
            }
        }
        @if !no_key {
            div.card {
                div.card-head { (icon("plus")) "New secret" }
                form.form method="post" action=(b) {
                    label { "Name" span.hint { "Letters, digits and _; not starting with a digit, GITHUB_ or FLUX_." }
                        input.mono type="text" name="name" required pattern="[A-Za-z_][A-Za-z0-9_]*" maxlength="100" autocomplete="off" placeholder="DEPLOY_TOKEN";
                    }
                    label { "Value" textarea.mono name="value" rows="4" required autocomplete="off" spellcheck="false" {} }
                    div.form-actions { button.btn.primary type="submit" { "Add secret" } }
                }
            }
        }
    })
}

// ───────────── danger zone ─────────────

/// Escape a string for an HTML `pattern` attribute (a JS regex).
fn regex_literal(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if "\\^$.|?*+()[]{}/-".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

pub fn danger_zone(ctx: &RepoCtx) -> Markup {
    let b = settings_base(ctx);
    let (owner, short) = match ctx.name.rsplit_once('/') {
        Some((o, s)) => (Some(o), s),
        None => (None, ctx.name.as_str()),
    };
    let archived = ctx.config.archived;
    let scanning = !ctx.config.secret_scanning_disabled;
    html! {
        h2.section-title { "Security" }
        div.card {
            div.list-row {
                div.list-main {
                    strong { "Push protection" " " @if scanning { span.pill.small.ok { "On" } } @else { span.pill.small { "Off" } } }
                    div.muted.small { "Refuse pushes and edits that leave credentials (cloud keys, tokens, private keys) in the repository. Mark a test fixture with " code { "flux:allow-secret" } " on its line." }
                }
                form method="post" action=(format!("{b}/push-protection")) data-confirm=[(scanning).then_some("Turn off push protection? Credentials will no longer be blocked.")] {
                    input type="hidden" name="enabled" value=(if scanning { "0" } else { "1" });
                    button.btn.small.danger[scanning].primary[!scanning] type="submit" { (if scanning { "Turn off" } else { "Turn on" }) }
                }
            }
        }
        h2.section-title { "Danger zone" }
        div.card.danger-zone {
            div.list-row {
                div.list-main { strong { "Rename" } div.muted.small { "Links to the old name stop working; update your remotes." } }
                form.inline-form method="post" action=(format!("{b}/rename")) {
                    @if let Some(o) = owner { span.muted.small { (o) "/" } }
                    input type="text" name="name" value=(short) required maxlength="100" pattern="[A-Za-z0-9][A-Za-z0-9._\\-]*" aria-label="New name";
                    button.btn.small type="submit" { "Rename" }
                }
            }
            div.list-row {
                div.list-main {
                    strong { @if archived { "Unarchive" } @else { "Archive" } }
                    div.muted.small { @if archived { "Make the repository writable again." } @else { "Make it read-only: no pushes, landing or new reviews." } }
                }
                form method="post" action=(format!("{b}/archive")) data-confirm=(if archived { "Unarchive this repository?" } else { "Archive this repository? It becomes read-only." }) {
                    input type="hidden" name="archived" value=(if archived { "0" } else { "1" });
                    button.btn.small.danger type="submit" { (icon("archive")) (if archived { "Unarchive" } else { "Archive" }) }
                }
            }
            div.list-row.wrap-sm {
                div.list-main { strong { "Delete this repository" } div.muted.small { "Removes its history, reviews, issues and releases. This can't be undone." } }
                form.inline-form method="post" action=(format!("{b}/delete")) {
                    input type="text" name="confirm" required pattern=(regex_literal(&ctx.name)) placeholder=(format!("Type {} to confirm", ctx.name)) aria-label=(format!("Type {} to confirm", ctx.name)) autocomplete="off" title=(format!("Type {} exactly", ctx.name));
                    button.btn.small.danger type="submit" { (icon("trash")) "Delete" }
                }
            }
        }
    }
}

// ───────────── posts ─────────────

fn safe_next<'a>(next: &'a str, fallback: &'a str) -> &'a str {
    if next.starts_with('/') && !next.starts_with("//") && !next.contains('\\') { next } else { fallback }
}

pub async fn post(web: &WebState, req: &Req, ctx: &RepoCtx, s: &[&str], f: &Fields) -> WebResult {
    let flux = &web.flux;
    let who = &req.principal;
    let sb = settings_base(ctx);
    match s {
        ["watch"] => {
            let u = who.user.as_ref().ok_or_else(|| WebError::Unauthorized("sign in to watch repositories".into()))?;
            let on = f.trim("state") == "watching";
            flux.meta.set_subscription(u.id, ctx.id, 0, if on { Some("subscribed") } else { None }).await?;
            let home = repo_url(&ctx.name);
            done(
                safe_next(f.get("next"), &home),
                Ok::<_, WebError>(
                    if on { "Watching: you'll be notified about all activity" } else { "No longer watching" }.into(),
                ),
            )
        }
        ["fork"] => {
            let me = match &who.user {
                Some(u) => u.name.clone(),
                None => return Ok(to(&format!("/-/login?next={}", enc(&repo_url(&ctx.name))))),
            };
            let short = ctx.name.rsplit('/').next().unwrap_or(&ctx.name);
            let dest = format!("{me}/{short}");
            match flux.fork_repo(who, ctx.id, &dest).await {
                Ok(_) => done(&repo_url(&dest), Ok::<_, WebError>(format!("Forked {} to {dest}", ctx.name))),
                Err(e) => done(&repo_url(&ctx.name), Err::<String, _>(e)),
            }
        }
        ["settings", "hooks"] => {
            let to_list = format!("{sb}/hooks");
            match flux.create_hook(who, Scope::Repo(ctx.id), hook_input(f, true)).await {
                Ok(h) => done(&format!("{to_list}/{}", h.id), Ok::<_, WebError>("Webhook added; a ping is on its way".into())),
                Err(e) => done(&to_list, Err::<String, _>(e)),
            }
        }
        ["settings", "hooks", id, rest @ ..] => {
            let id = parse_num(id)?;
            let back = format!("{sb}/hooks/{id}");
            let scope = Scope::Repo(ctx.id);
            match rest {
                [] => done(
                    &back,
                    flux.update_hook(who, scope, id, hook_input(f, false)).await.map(|_| "Webhook updated".to_string()),
                ),
                ["delete"] => {
                    done(&format!("{sb}/hooks"), flux.delete_hook(who, scope, id).await.map(|_| "Webhook deleted".to_string()))
                }
                ["ping"] => {
                    let r = async {
                        let h = flux.hook_in(who, scope, id).await?;
                        flux.ping_hook(&h).await
                    }
                    .await;
                    done(&back, r.map(|_| "Ping queued".to_string()))
                }
                ["deliveries", d, "redeliver"] => {
                    let d = parse_num(d)?;
                    done(&back, flux.redeliver(who, scope, id, d).await.map(|_| "Redelivery queued".to_string()))
                }
                _ => Err(WebError::NotFound("no such action".into())),
            }
        }
        ["settings", "branches"] => {
            let back = format!("{sb}/branches");
            let r = async {
                flux.authorize(who, ctx.id, Role::Admin).await?;
                let mut cfg = (*flux.repo_config(ctx.id).await?).clone();
                let mut rules = cfg.branch_rules.clone().unwrap_or_else(crate::protect::default_rules);
                let idx: Option<usize> = f.trim("index").parse().ok();
                let msg = match f.trim("action") {
                    "reset" => {
                        cfg.branch_rules = None;
                        "Restored the default rules"
                    }
                    "add" => {
                        let r = rule_from(f)?;
                        if rules.iter().any(|x| x.pattern == r.pattern) {
                            return Err(WebError::Conflict(format!("a rule for {} already exists", r.pattern)));
                        }
                        rules.push(r);
                        cfg.branch_rules = Some(rules);
                        "Rule added"
                    }
                    "delete" => {
                        let i = idx.filter(|i| *i < rules.len()).ok_or_else(|| WebError::BadRequest("no such rule".into()))?;
                        rules.remove(i);
                        cfg.branch_rules = Some(rules);
                        "Rule deleted"
                    }
                    _ => {
                        let i = idx.filter(|i| *i < rules.len()).ok_or_else(|| WebError::BadRequest("no such rule".into()))?;
                        rules[i] = rule_from(f)?;
                        cfg.branch_rules = Some(rules);
                        "Rule saved"
                    }
                };
                flux.set_repo_config(ctx.id, &cfg).await?;
                flux.audit(who, "repo.protection", &ctx.name, &serde_json::to_string(&cfg.branch_rules).unwrap_or_default())
                    .await;
                Ok::<String, WebError>(msg.into())
            }
            .await;
            done(&back, r)
        }
        ["settings", "secrets", rest @ ..] => {
            let back = format!("{sb}/secrets");
            let scope = ("repo", ctx.id.0);
            let name = f.trim("name");
            match rest {
                [] => done(&back, flux.put_secret(who, scope, name, f.get("value")).await.map(|_| format!("Saved {name}"))),
                ["delete"] => done(&back, flux.delete_secret(who, scope, name).await.map(|_| format!("Deleted {name}"))),
                _ => Err(WebError::NotFound("no such action".into())),
            }
        }
        ["settings", "rename"] => {
            let short = f.trim("name");
            let new = match ctx.name.rsplit_once('/') {
                Some((o, _)) => format!("{o}/{short}"),
                None => short.to_string(),
            };
            if new == ctx.name {
                return done(&sb, Ok::<_, WebError>(String::new()));
            }
            match flux.rename_repo_as(who, ctx.id, &new).await {
                Ok(()) => done(&format!("/{}/-/settings", enc(&new)), Ok::<_, WebError>(format!("Renamed to {new}"))),
                Err(e) => done(&sb, Err::<String, _>(e)),
            }
        }
        ["settings", "push-protection"] => {
            let r = async {
                flux.authorize(who, ctx.id, Role::Admin).await?;
                let mut cfg = (*flux.repo_config(ctx.id).await?).clone();
                cfg.secret_scanning_disabled = f.trim("enabled") != "1";
                flux.set_repo_config(ctx.id, &cfg).await?;
                let on = !cfg.secret_scanning_disabled;
                flux.audit(who, if on { "repo.push_protection.on" } else { "repo.push_protection.off" }, &ctx.name, "").await;
                Ok::<String, WebError>(if on { "Push protection is on" } else { "Push protection is off" }.into())
            }
            .await;
            done(&sb, r)
        }
        ["settings", "archive"] => {
            let r = async {
                flux.authorize(who, ctx.id, Role::Admin).await?;
                let mut cfg = (*flux.repo_config(ctx.id).await?).clone();
                cfg.archived = f.trim("archived") == "1";
                flux.set_repo_config(ctx.id, &cfg).await?;
                flux.audit(who, if cfg.archived { "repo.archive" } else { "repo.unarchive" }, &ctx.name, "").await;
                Ok::<String, WebError>(if cfg.archived { "Repository archived" } else { "Repository unarchived" }.into())
            }
            .await;
            done(&sb, r)
        }
        ["settings", "delete"] => {
            if f.trim("confirm") != ctx.name {
                return done(&sb, Err::<String, _>(WebError::BadRequest(format!("type {} to confirm", ctx.name))));
            }
            match flux.delete_repo_as(who, ctx.id).await {
                Ok(()) => Ok(to("/")),
                Err(e) => done(&sb, Err::<String, _>(e)),
            }
        }
        _ => Err(WebError::NotFound("no such action".into())),
    }
}
