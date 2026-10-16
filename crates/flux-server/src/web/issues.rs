//! Issues, labels and milestones, and the label/people forms shared with
//! reviews.

use std::collections::HashMap;

use axum::response::{IntoResponse, Redirect};
use flux_store::{IssueQuery, Label, Milestone};
use maud::{Markup, PreEscaped, html};

use super::forms::{Fields, done, to};
use super::markdown;
use super::pages::{RepoCtx, nav, parse_num};
use super::view::{self, Tab, avatar, enc, icon, label_pill, page, time};
use super::{Req, WebError, WebResult, WebState};
use crate::issues::{IssueEdit, IssueInput, IssueJson};
use crate::rbac::Role;

const PER_PAGE: u32 = 25;

fn base(ctx: &RepoCtx) -> String {
    format!("/{}/-", enc(&ctx.name))
}

fn default_rev(ctx: &RepoCtx) -> String {
    ctx.default_branch().unwrap_or("main").to_string()
}

/// `YYYY-MM-DD` (UTC midnight) → unix milliseconds.
pub fn parse_date(s: &str) -> Option<i64> {
    let mut it = s.trim().splitn(3, '-');
    let (y, m, d): (i64, i64, i64) = (it.next()?.parse().ok()?, it.next()?.parse().ok()?, it.next()?.parse().ok()?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some((era * 146_097 + doe - 719_468) * 86_400_000)
}

fn date_value(ms: i64) -> String {
    view::iso(ms)[..10].to_string()
}

/// Label colors by name, for pills.
async fn label_colors(web: &WebState, ctx: &RepoCtx) -> WebResult<(Vec<Label>, HashMap<String, String>)> {
    let labels = web.flux.meta.list_labels(ctx.id).await?;
    let map = labels.iter().map(|l| (l.name.clone(), l.color.clone())).collect();
    Ok((labels, map))
}

fn pill(colors: &HashMap<String, String>, name: &str) -> Markup {
    label_pill(name, colors.get(name).map(String::as_str).unwrap_or("8a8f98"))
}

fn issue_state_icon(state: &str, reason: &str) -> Markup {
    let (cls, ic, title) = match (state, reason) {
        ("open", _) => ("open", "issue", "Open"),
        (_, "not_planned" | "duplicate") => ("skipped", "issue-closed", "Closed as not planned"),
        _ => ("done", "issue-closed", "Closed"),
    };
    html! { span class=(format!("issue-icon {cls}")) title=(title) aria-label=(title) { (icon(ic)) } }
}

fn issue_badge(state: &str, reason: &str) -> Markup {
    let (cls, ic, text) = match (state, reason) {
        ("open", _) => ("open", "issue", "Open"),
        (_, "not_planned") => ("closed-np", "issue-closed", "Closed · not planned"),
        (_, "duplicate") => ("closed-np", "issue-closed", "Closed · duplicate"),
        _ => ("landed", "issue-closed", "Closed"),
    };
    html! { span class=(format!("state {cls}")) { (icon(ic)) (text) } }
}

// ───────────── list ─────────────

#[derive(Clone, Default)]
struct Filters {
    state: String,
    q: String,
    label: String,
    assignee: String,
    author: String,
    milestone: String,
    sort: String,
}

impl Filters {
    fn from(q: &HashMap<String, String>) -> Self {
        let g = |k: &str| q.get(k).map(|v| v.trim().to_string()).unwrap_or_default();
        let state = match g("state").as_str() {
            "closed" => "closed",
            "all" => "all",
            _ => "open",
        };
        Self {
            state: state.into(),
            q: g("q"),
            label: g("label"),
            assignee: g("assignee"),
            author: g("author"),
            milestone: g("milestone"),
            sort: g("sort"),
        }
    }

    fn any_filter(&self) -> bool {
        !(self.q.is_empty()
            && self.label.is_empty()
            && self.assignee.is_empty()
            && self.author.is_empty()
            && self.milestone.is_empty())
    }

    fn query(&self, state: Option<&str>, limit: u32, offset: u32) -> IssueQuery {
        let opt = |s: &str| Some(s.to_string()).filter(|s| !s.is_empty());
        IssueQuery {
            state: state.map(str::to_string),
            label: opt(&self.label),
            assignee: opt(&self.assignee),
            author: opt(&self.author),
            milestone: self.milestone.parse().ok(),
            text: opt(&self.q),
            sort: opt(&self.sort),
            limit,
            offset,
        }
    }

    /// This listing's URL with one field changed (`None` clears it).
    fn url(&self, ctx: &RepoCtx, key: &str, val: Option<&str>) -> String {
        let mut f = self.clone();
        let v = val.unwrap_or("").to_string();
        match key {
            "state" => f.state = v,
            "q" => f.q = v,
            "label" => f.label = v,
            "assignee" => f.assignee = v,
            "author" => f.author = v,
            "milestone" => f.milestone = v,
            "sort" => f.sort = v,
            _ => {}
        }
        let mut parts = Vec::new();
        for (k, v) in [
            ("state", &f.state),
            ("q", &f.q),
            ("label", &f.label),
            ("assignee", &f.assignee),
            ("author", &f.author),
            ("milestone", &f.milestone),
            ("sort", &f.sort),
        ] {
            if !(v.is_empty() || (k == "state" && v == "open")) {
                parts.push(format!("{k}={}", enc(v)));
            }
        }
        let b = format!("{}/issues", base(ctx));
        if parts.is_empty() { b } else { format!("{b}?{}", parts.join("&")) }
    }
}

pub async fn issues_get(web: &WebState, req: &Req, ctx: &RepoCtx, args: &[String], q: &HashMap<String, String>) -> WebResult {
    match args {
        [] => list(web, req, ctx, q).await,
        [n] if n == "new" => new_issue(web, req, ctx).await,
        [n] => detail(web, req, ctx, parse_num(n)?).await,
        _ => Err(WebError::NotFound("no such page".into())),
    }
}

async fn list(web: &WebState, req: &Req, ctx: &RepoCtx, q: &HashMap<String, String>) -> WebResult {
    let flux = &web.flux;
    let f = Filters::from(q);
    let page_no: u32 = q.get("page").and_then(|p| p.parse().ok()).unwrap_or(0).min(10_000);
    let state = (f.state != "all").then_some(f.state.as_str());
    let (items, total) = flux.list_issues_as(&req.principal, ctx.id, &f.query(state, PER_PAGE, page_no * PER_PAGE)).await?;
    let (open, closed) = if f.any_filter() {
        let o = flux.list_issues_as(&req.principal, ctx.id, &f.query(Some("open"), 1, 0)).await?.1;
        let c = flux.list_issues_as(&req.principal, ctx.id, &f.query(Some("closed"), 1, 0)).await?.1;
        (o, c)
    } else {
        flux.meta.issue_counts(ctx.id).await?
    };
    let (labels, colors) = label_colors(web, ctx).await?;
    let milestones = flux.meta.list_milestones(ctx.id).await?;
    let ms_title: HashMap<i64, &str> = milestones.iter().map(|m| (m.number, m.title.as_str())).collect();
    let me = req.principal.user.as_ref().map(|u| u.name.clone());
    let b = base(ctx);
    let pages = (total as u32).div_ceil(PER_PAGE);
    let body = html! {
        div.container {
            div.page-head.row {
                h1 { "Issues" }
                div.spacer {}
                a.btn href=(format!("{b}/labels")) { (icon("tag")) "Labels" }
                a.btn href=(format!("{b}/milestones")) { (icon("milestone")) "Milestones" }
                a.btn.primary href=(format!("{b}/issues/new")) { (icon("plus")) "New issue" }
            }
            form.search-form.issue-search method="get" action=(format!("{b}/issues")) {
                div.search-row {
                    (icon("search"))
                    input type="search" name="q" value=(f.q) placeholder="Search issues" aria-label="Search issues";
                    @if f.state != "open" { input type="hidden" name="state" value=(f.state); }
                    @for (k, v) in [("label", &f.label), ("assignee", &f.assignee), ("author", &f.author), ("milestone", &f.milestone)] {
                        @if !v.is_empty() { input type="hidden" name=(k) value=(v); }
                    }
                    select name="sort" aria-label="Sort" {
                        option value="" selected[f.sort.is_empty() || f.sort == "updated"] { "Recently updated" }
                        option value="created" selected[f.sort == "created"] { "Newest" }
                        option value="comments" selected[f.sort == "comments"] { "Most commented" }
                    }
                    button.btn.primary type="submit" { "Search" }
                }
            }
            @if f.any_filter() {
                div.active-filters {
                    span.muted.small { "Filtered by" }
                    @for (k, label, v) in [("q", "text", &f.q), ("label", "label", &f.label), ("assignee", "assignee", &f.assignee), ("author", "author", &f.author), ("milestone", "milestone", &f.milestone)] {
                        @if !v.is_empty() {
                            a.filter-chip href=(f.url(ctx, k, None)) title="Remove filter" {
                                span.muted { (label) ": " }
                                @if k == "milestone" { (v.parse::<i64>().ok().and_then(|n| ms_title.get(&n).copied()).unwrap_or(v)) } @else { (v) }
                                (icon("x"))
                            }
                        }
                    }
                    a.link.small href=(format!("{b}/issues")) { "Clear all" }
                }
            }
            div.card {
                div.filter-tabs.issue-tabs {
                    a.active[f.state == "open"] href=(f.url(ctx, "state", Some("open"))) { (icon("issue")) "Open" span.count { (open) } }
                    a.active[f.state == "closed"] href=(f.url(ctx, "state", Some("closed"))) { (icon("issue-closed")) "Closed" span.count { (closed) } }
                    div.spacer {}
                    details.menu.filter-menu {
                        summary.btn.small.ghost { "Label" span.caret {} }
                        div.menu-pop {
                            @if labels.is_empty() { div.menu-head { "No labels yet" } }
                            @for l in &labels {
                                a.active[f.label == l.name] href=(f.url(ctx, "label", Some(&l.name))) { (label_pill(&l.name, &l.color)) }
                            }
                        }
                    }
                    details.menu.filter-menu {
                        summary.btn.small.ghost { "Assignee" span.caret {} }
                        div.menu-pop {
                            @if let Some(u) = &me {
                                a href=(f.url(ctx, "assignee", Some(u))) { (icon("user")) "Assigned to me" }
                                a href=(f.url(ctx, "author", Some(u))) { (icon("edit")) "Opened by me" }
                            }
                            form.menu-form method="get" action=(format!("{b}/issues")) {
                                @if f.state != "open" { input type="hidden" name="state" value=(f.state); }
                                input type="text" name="assignee" placeholder="Username" aria-label="Filter by assignee";
                            }
                        }
                    }
                    @if !milestones.is_empty() {
                        details.menu.filter-menu {
                            summary.btn.small.ghost { "Milestone" span.caret {} }
                            div.menu-pop {
                                @for m in &milestones {
                                    a href=(f.url(ctx, "milestone", Some(&m.number.to_string()))) { (icon("milestone")) (m.title) }
                                }
                            }
                        }
                    }
                }
                @if items.is_empty() {
                    div.empty.small {
                        h2 { @if f.any_filter() { "No matching issues" } @else if f.state == "closed" { "No closed issues" } @else { "No open issues" } }
                        p.muted { "Issues track bugs, ideas and work to do. Anyone who can read the repository can open one." }
                    }
                }
                ul.issues {
                    @for i in &items { (issue_row(ctx, &f, &colors, &ms_title, i)) }
                }
            }
            @if pages > 1 {
                div.pager {
                    @if page_no > 0 { a.btn href=(page_url(&f.url(ctx, "", None), page_no - 1)) { "← Newer" } }
                    span.muted.small { "Page " (page_no + 1) " of " (pages) }
                    @if page_no + 1 < pages { a.btn href=(page_url(&f.url(ctx, "", None), page_no + 1)) { "Older →" } }
                }
            }
        }
    };
    let rev = default_rev(ctx);
    let n = nav(web, req, ctx, Tab::Issues, &rev).await;
    Ok(page(web, req, &format!("Issues · {}", ctx.name), Some(n), body))
}

fn page_url(u: &str, p: u32) -> String {
    format!("{u}{}page={p}", if u.contains('?') { "&" } else { "?" })
}

fn issue_row(ctx: &RepoCtx, f: &Filters, colors: &HashMap<String, String>, ms: &HashMap<i64, &str>, i: &IssueJson) -> Markup {
    html! {
        li.issue-row {
            (issue_state_icon(&i.state, &i.state_reason))
            div.issue-main {
                div.issue-line {
                    a.issue-title href=(format!("{}/issues/{}", base(ctx), i.number)) { (i.title) }
                    @for l in &i.labels { a.label-link href=(f.url(ctx, "label", Some(l))) { (pill(colors, l)) } }
                }
                div.muted.small {
                    "#" (i.number) " "
                    @match i.closed_at.filter(|_| i.state != "open") {
                        Some(c) => { "by " (i.author) " · closed " (time(c)) }
                        None => { "opened " (time(i.created_at)) " by " (i.author) }
                    }
                    @if let Some(m) = i.milestone.and_then(|m| ms.get(&m)) { span.ms-tag { (icon("milestone")) (m) } }
                }
            }
            div.issue-side {
                @if !i.assignees.is_empty() {
                    span.avatars { @for a in i.assignees.iter().take(3) { (avatar(a, "", false)) } }
                }
                @if i.comments > 0 {
                    a.comments href=(format!("{}/issues/{}", base(ctx), i.number)) title=(format!("{} comments", i.comments)) { (icon("comment")) (i.comments) }
                }
            }
        }
    }
}

// ───────────── one issue ─────────────

async fn detail(web: &WebState, req: &Req, ctx: &RepoCtx, number: i64) -> WebResult {
    let flux = &web.flux;
    if flux.meta.item_kind(ctx.id, number).await? == Some("review") {
        return Ok(Redirect::to(&format!("{}/reviews/{number}", base(ctx))).into_response());
    }
    let v = flux.issue_view(&req.principal, ctx.id, number).await?;
    let i = &v.issue;
    let (labels, colors) = label_colors(web, ctx).await?;
    let milestones = flux.meta.list_milestones(ctx.id).await?;
    let me = req.principal.user.as_ref();
    let is_author = me.is_some_and(|u| u.name == i.author);
    let triage = ctx.can(Role::Triage);
    let can_edit = is_author || triage;
    let subscribed = match me {
        Some(u) => flux.meta.subscription(u.id, ctx.id, number).await?.as_deref() == Some("subscribed"),
        None => false,
    };
    let b = format!("{}/issues/{number}", base(ctx));
    let can_comment = me.is_some() && (!i.locked || triage);
    let milestone = i.milestone.and_then(|n| milestones.iter().find(|m| m.number == n));
    let body = html! {
        div.container {
            div.review-head {
                div.title-row {
                    h1 { (i.title) span.muted { " #" (i.number) } }
                    @if can_edit {
                        details.edit-toggle {
                            summary.btn.small { (icon("edit")) "Edit" }
                            form.card.form.edit-pop method="post" action=(format!("{b}/edit")) {
                                label { "Title" input type="text" name="title" value=(i.title) required maxlength="256"; }
                                label { "Description " span.muted { "(Markdown)" } textarea name="body" rows="8" { (i.body) } }
                                div.form-actions { button.btn.primary type="submit" { "Save" } }
                            }
                        }
                    }
                }
                div.review-sub {
                    (issue_badge(&i.state, &i.state_reason))
                    strong { (i.author) }
                    span.muted { " opened this " (time(i.created_at)) " · " (i.comments) " comment" @if i.comments != 1 { "s" } }
                    @if i.locked { span.pill.small { (icon("lock")) "Locked" } }
                }
            }
            div.review-grid {
                div.timeline {
                    div.card.comment id="issue-body" {
                        div.comment-head { (avatar(&i.author, "", false)) strong { (i.author) } span.muted { " opened " (time(i.created_at)) } @if is_author { span.pill.small { "Author" } } }
                        div.comment-body {
                            @if i.body.trim().is_empty() { p.muted { "No description provided." } }
                            @else { article.markdown { (PreEscaped(markdown::render(&i.body, None))) } }
                        }
                    }
                    @for e in &v.timeline {
                        @if e.kind == "comment" {
                            div.card.comment id=(format!("c{}", e.id)) {
                                div.comment-head {
                                    (avatar(&e.author, "", false)) strong { (e.author) }
                                    a.muted href=(format!("#c{}", e.id)) { " commented " (time(e.created_at)) }
                                    @if e.updated_at > e.created_at + 1000 { span.faint.small { " · edited" } }
                                    @if e.author == i.author { span.pill.small { "Author" } }
                                    div.spacer {}
                                    @if me.is_some_and(|u| u.name == e.author) || triage {
                                        details.menu.comment-menu {
                                            summary.btn.small.ghost.icon-only aria-label="Comment actions" { "⋯" }
                                            div.menu-pop.wide {
                                                form method="post" action=(format!("{b}/comments/{}/edit", e.id)) {
                                                    textarea name="body" rows="5" required aria-label="Edit comment" { (e.body) }
                                                    div.form-actions { button.btn.small.primary type="submit" { "Update comment" } }
                                                }
                                                form method="post" action=(format!("{b}/comments/{}/delete", e.id)) data-confirm="Delete this comment?" {
                                                    button.danger-item type="submit" { (icon("trash")) "Delete" }
                                                }
                                            }
                                        }
                                    }
                                }
                                div.comment-body { article.markdown { (PreEscaped(markdown::render(&e.body, None))) } }
                            }
                        } @else {
                            (timeline_event(&e.kind, &e.author, &e.body, e.created_at, &colors, &milestones))
                        }
                    }
                    @if can_comment {
                        form.card.comment-form method="post" action=(format!("{b}/comment")) {
                            textarea name="body" rows="5" placeholder="Leave a comment (Markdown)" aria-label="Comment" {}
                            div.form-actions {
                                @if can_edit {
                                    @if i.state == "open" {
                                        select.small-select name="reason" aria-label="Close reason" {
                                            option value="completed" { "Completed" }
                                            option value="not_planned" { "Not planned" }
                                            option value="duplicate" { "Duplicate" }
                                        }
                                        button.btn type="submit" name="state" value="closed" { (icon("issue-closed")) "Close issue" }
                                    } @else {
                                        button.btn type="submit" name="state" value="open" { (icon("issue")) "Reopen" }
                                    }
                                }
                                button.btn.primary type="submit" name="comment" value="1" { "Comment" }
                            }
                        }
                    } @else if me.is_none() {
                        div.notice.small.rounded { a.link href=(format!("/-/login?next={}", enc(&req.path))) { "Sign in" } " to join the conversation." }
                    } @else {
                        div.notice.small.rounded { (icon("lock")) " This conversation is locked to collaborators." }
                    }
                }
                aside.review-side {
                    (side_people("Assignees", "assignees", &b, &i.assignees, triage, "No one assigned"))
                    div.side-block {
                        div.side-title.row { "Labels" div.spacer {}
                            @if triage { (labels_editor(&b, &labels, &i.labels)) }
                        }
                        @if i.labels.is_empty() { span.muted { "None yet" } }
                        div.label-list { @for l in &i.labels { a href=(format!("{}/issues?label={}", base(ctx), enc(l))) { (pill(&colors, l)) } } }
                    }
                    div.side-block {
                        div.side-title.row { "Milestone" div.spacer {}
                            @if triage {
                                details.side-edit {
                                    summary.link.small { "Edit" }
                                    form.side-form method="post" action=(format!("{b}/milestone")) {
                                        select name="milestone" aria-label="Milestone" {
                                            option value="" { "No milestone" }
                                            @for m in milestones.iter().filter(|m| m.state == "open" || Some(m.number) == i.milestone) {
                                                option value=(m.number) selected[Some(m.number) == i.milestone] { (m.title) }
                                            }
                                        }
                                        button.btn.small type="submit" { "Save" }
                                    }
                                }
                            }
                        }
                        @match milestone {
                            Some(m) => { a.ms-side href=(format!("{}/issues?milestone={}&state=all", base(ctx), m.number)) { (icon("milestone")) (m.title) } (progress(m)) }
                            None => { span.muted { "None" } }
                        }
                    }
                    @if me.is_some() {
                        div.side-block {
                            div.side-title { "Notifications" }
                            form method="post" action=(format!("{b}/subscribe")) {
                                input type="hidden" name="state" value=(if subscribed { "none" } else { "subscribed" });
                                button.btn.small.wide type="submit" { (icon("bell")) (if subscribed { "Unsubscribe" } else { "Subscribe" }) }
                            }
                            p.muted.small { @if subscribed { "You're receiving notifications for this issue." } @else { "You're not receiving notifications for this issue." } }
                        }
                    }
                    @if triage {
                        div.side-block {
                            form method="post" action=(format!("{b}/lock")) {
                                input type="hidden" name="locked" value=(if i.locked { "0" } else { "1" });
                                button.btn.small.ghost type="submit" { (icon(if i.locked { "lock-open" } else { "lock" })) (if i.locked { "Unlock conversation" } else { "Lock conversation" }) }
                            }
                        }
                    }
                }
            }
        }
    };
    let rev = default_rev(ctx);
    let n = nav(web, req, ctx, Tab::Issues, &rev).await;
    Ok(page(web, req, &format!("{} · Issue #{}", i.title, i.number), Some(n), body))
}

/// A compact timeline line for a non-comment event.
fn timeline_event(
    kind: &str,
    author: &str,
    body: &str,
    at: i64,
    colors: &HashMap<String, String>,
    milestones: &[Milestone],
) -> Markup {
    let ic = match kind {
        "closed" => "issue-closed",
        "reopened" => "issue",
        "labeled" | "unlabeled" => "tag",
        "assigned" | "unassigned" => "user",
        "renamed" => "edit",
        "milestoned" | "demilestoned" => "milestone",
        "locked" => "lock",
        "unlocked" => "lock-open",
        _ => "pulse",
    };
    let ms_name =
        |b: &str| b.parse::<i64>().ok().and_then(|n| milestones.iter().find(|m| m.number == n)).map(|m| m.title.clone());
    html! {
        div.event {
            span class=(format!("event-icon {kind}")) { (icon(ic)) }
            strong { (author) }
            span.muted {
                @match kind {
                    "closed" => { " closed this" @match body { "not_planned" => { " as not planned" } "duplicate" => { " as a duplicate" } _ => { " as completed" } } }
                    "reopened" => { " reopened this" }
                    "labeled" => { " added " (pill(colors, body)) }
                    "unlabeled" => { " removed " (pill(colors, body)) }
                    "assigned" => { " assigned " strong { (body) } }
                    "unassigned" => { " unassigned " strong { (body) } }
                    "renamed" => {
                        @match body.split_once(" → ") {
                            Some((old, new)) => { " changed the title " del { (old) } " " strong { (new) } }
                            None => { " changed the title" }
                        }
                    }
                    "milestoned" => { " added this to " strong { (ms_name(body).unwrap_or_else(|| body.to_string())) } }
                    "demilestoned" => { " removed this from a milestone" }
                    "locked" => { " locked the conversation" }
                    "unlocked" => { " unlocked the conversation" }
                    k => { " " (k.replace('_', " ")) @if !body.is_empty() { " " (body) } }
                }
                " " (time(at))
            }
        }
    }
}

/// A sidebar block listing people, editable by triagers.
fn side_people(title: &str, what: &str, action_base: &str, people: &[String], editable: bool, empty: &str) -> Markup {
    html! {
        div.side-block {
            div.side-title.row { (title) div.spacer {}
                @if editable {
                    details.side-edit {
                        summary.link.small { "Edit" }
                        form.side-form method="post" action=(format!("{action_base}/{what}")) {
                            input type="text" name="names" value=(people.join(", ")) placeholder="alice, bob" aria-label=(title);
                            button.btn.small type="submit" { "Save" }
                        }
                    }
                }
            }
            @if people.is_empty() { span.muted { (empty) } }
            @for p in people { div.person { (avatar(p, "", false)) a href=(view::repo_url(p)) { (p) } } }
        }
    }
}

/// Checkbox list of labels (triage).
fn labels_editor(action_base: &str, all: &[Label], current: &[String]) -> Markup {
    html! {
        details.side-edit {
            summary.link.small { "Edit" }
            form.side-form.label-form method="post" action=(format!("{action_base}/labels")) {
                input type="hidden" name="labels" value="";
                @if all.is_empty() { p.muted.small { "No labels yet." } }
                div.label-checks {
                    @for l in all {
                        label.check { input type="checkbox" name="labels" value=(l.name) checked[current.contains(&l.name)]; (label_pill(&l.name, &l.color)) }
                    }
                }
                button.btn.small type="submit" { "Apply" }
            }
        }
    }
}

fn progress(m: &Milestone) -> Markup {
    let total = m.open_items + m.closed_items;
    let pct = if total > 0 { m.closed_items * 100 / total } else { 0 };
    html! {
        div.progress-row {
            progress.bar value=(m.closed_items) max=(total.max(1)) aria-label=(format!("{pct}% complete")) {}
            span.muted.small { (pct) "%" }
        }
    }
}

async fn new_issue(web: &WebState, req: &Req, ctx: &RepoCtx) -> WebResult {
    if req.principal.user.is_none() {
        return Ok(to(&format!("/-/login?next={}", enc(&req.path))));
    }
    if ctx.config.issues_disabled {
        return Err(WebError::NotFound("issues are turned off for this repository".into()));
    }
    let triage = ctx.can(Role::Triage);
    let labels = web.flux.meta.list_labels(ctx.id).await?;
    let milestones = web.flux.meta.list_milestones(ctx.id).await?;
    let body = html! {
        div.container.narrow {
            div.page-head { h1 { "New issue" } p.muted { "Describe a bug, an idea or a task. Markdown works." } }
            form.card.form method="post" action=(format!("{}/issues", base(ctx))) {
                label { "Title" input type="text" name="title" required autofocus maxlength="256" placeholder="Short summary"; }
                label { "Description " span.muted { "(Markdown)" } textarea name="body" rows="10" placeholder="Steps to reproduce, expected behaviour, context…" {} }
                @if triage {
                    div.row2 {
                        label { "Assignees" span.hint { "Usernames, comma separated." } input type="text" name="assignees" placeholder="alice, bob"; }
                        label { "Milestone"
                            select name="milestone" {
                                option value="" { "No milestone" }
                                @for m in milestones.iter().filter(|m| m.state == "open") { option value=(m.number) { (m.title) } }
                            }
                        }
                    }
                    @if !labels.is_empty() {
                        fieldset.label-field {
                            legend { "Labels" }
                            div.label-checks.wrap {
                                @for l in &labels { label.check { input type="checkbox" name="labels" value=(l.name); (label_pill(&l.name, &l.color)) } }
                            }
                        }
                    }
                }
                div.form-actions { a.btn href=(format!("{}/issues", base(ctx))) { "Cancel" } button.btn.primary type="submit" { "Open issue" } }
            }
        }
    };
    let rev = default_rev(ctx);
    let n = nav(web, req, ctx, Tab::Issues, &rev).await;
    Ok(page(web, req, &format!("New issue · {}", ctx.name), Some(n), body))
}

// ───────────── labels ─────────────

/// A pleasant default for new labels, varied by the current count.
fn suggested_color(i: usize) -> &'static str {
    const C: [&str; 8] = ["3466d6", "52a7a0", "a580c4", "c4a14f", "6aab7b", "cc7a75", "5c9fd6", "8b929c"];
    C[i % C.len()]
}

pub async fn labels_page(web: &WebState, req: &Req, ctx: &RepoCtx) -> WebResult {
    let labels = web.flux.meta.list_labels(ctx.id).await?;
    let can_write = ctx.can(Role::Write);
    let b = base(ctx);
    let body = html! {
        div.container {
            div.page-head.row {
                div { h1 { "Labels" } p.muted { "Labels sort issues and reviews. Triagers apply them; writers manage them." } }
                div.spacer {}
                a.btn href=(format!("{b}/issues")) { (icon("issue")) "Issues" }
                a.btn href=(format!("{b}/milestones")) { (icon("milestone")) "Milestones" }
            }
            @if can_write {
                form.card.form.row-form method="post" action=(format!("{b}/labels")) {
                    label { "Color" input.color-input type="color" name="color" value=(format!("#{}", suggested_color(labels.len()))); }
                    label.grow { "Name" input type="text" name="name" required maxlength="50" placeholder="needs design"; }
                    label.grow { "Description" input type="text" name="description" maxlength="100" placeholder="Optional"; }
                    div.form-actions { button.btn.primary type="submit" { (icon("plus")) "Create label" } }
                }
            }
            div.card {
                div.card-head { (icon("tag")) "Labels" span.count { (labels.len()) } }
                @if labels.is_empty() { div.pad.muted { "No labels yet." } }
                @for l in &labels {
                    div.list-row.label-row {
                        div.label-name { a href=(format!("{b}/issues?label={}", enc(&l.name))) { (label_pill(&l.name, &l.color)) } }
                        div.list-main.muted.small { (l.description) }
                        @if can_write {
                            details.menu.row-menu {
                                summary.btn.small { (icon("edit")) "Edit" }
                                div.menu-pop.wide {
                                    form.form-tight method="post" action=(format!("{b}/labels/edit")) {
                                        input type="hidden" name="old" value=(l.name);
                                        label { "Name" input type="text" name="name" value=(l.name) required maxlength="50"; }
                                        label { "Description" input type="text" name="description" value=(l.description) maxlength="100"; }
                                        label { "Color" input.color-input type="color" name="color" value=(format!("#{}", l.color)); }
                                        div.form-actions { button.btn.small.primary type="submit" { "Save label" } }
                                    }
                                }
                            }
                            form method="post" action=(format!("{b}/labels/delete")) data-confirm=(format!("Delete the label {:?}? It is removed from every issue and review.", l.name)) {
                                input type="hidden" name="name" value=(l.name);
                                button.btn.small.danger type="submit" aria-label=(format!("Delete {}", l.name)) { (icon("trash")) }
                            }
                        }
                    }
                }
            }
        }
    };
    let rev = default_rev(ctx);
    let n = nav(web, req, ctx, Tab::Issues, &rev).await;
    Ok(page(web, req, &format!("Labels · {}", ctx.name), Some(n), body))
}

// ───────────── milestones ─────────────

pub async fn milestones_page(web: &WebState, req: &Req, ctx: &RepoCtx, q: &HashMap<String, String>) -> WebResult {
    let all = web.flux.meta.list_milestones(ctx.id).await?;
    let state = if q.get("state").map(String::as_str) == Some("closed") { "closed" } else { "open" };
    let shown: Vec<&Milestone> = all.iter().filter(|m| m.state == state).collect();
    let can_write = ctx.can(Role::Write);
    let b = base(ctx);
    let count = |s: &str| all.iter().filter(|m| m.state == s).count();
    let body = html! {
        div.container {
            div.page-head.row {
                div { h1 { "Milestones" } p.muted { "Group issues and reviews toward a goal or a date." } }
                div.spacer {}
                a.btn href=(format!("{b}/issues")) { (icon("issue")) "Issues" }
                a.btn href=(format!("{b}/labels")) { (icon("tag")) "Labels" }
            }
            @if can_write {
                details.card.disclosure {
                    summary { (icon("plus")) "New milestone" }
                    form.form method="post" action=(format!("{b}/milestones")) {
                        div.row2 {
                            label { "Title" input type="text" name="title" required maxlength="200" placeholder="v1.0"; }
                            label { "Due date " span.muted { "(optional)" } input type="date" name="due"; }
                        }
                        label { "Description" textarea name="description" rows="3" {} }
                        div.form-actions { button.btn.primary type="submit" { "Create milestone" } }
                    }
                }
            }
            div.card {
                div.filter-tabs {
                    a.active[state == "open"] href=(format!("{b}/milestones")) { (icon("milestone")) "Open" span.count { (count("open")) } }
                    a.active[state == "closed"] href=(format!("{b}/milestones?state=closed")) { (icon("check")) "Closed" span.count { (count("closed")) } }
                }
                @if shown.is_empty() { div.empty.small { p.muted { "No " (state) " milestones." } } }
                @for m in &shown {
                    div.milestone-row {
                        div.ms-main {
                            a.ms-title href=(format!("{b}/issues?milestone={}&state=all", m.number)) { (m.title) }
                            div.muted.small {
                                @match m.due_ms {
                                    Some(d) if m.state == "open" && d < view::now_ms() => { span.overdue { (icon("alert")) "Past due by " (view::ago(d).trim_end_matches(" ago")) } }
                                    Some(d) => { (icon("clock")) " Due " (view::day_label(d)) }
                                    None => { "No due date" }
                                }
                                @if let Some(c) = m.closed_ms { " · closed " (time(c)) }
                            }
                            @if !m.description.is_empty() { p.ms-desc { (m.description) } }
                        }
                        div.ms-progress {
                            (progress(m))
                            div.muted.small {
                                a href=(format!("{b}/issues?milestone={}", m.number)) { (m.open_items) " open" } " · "
                                a href=(format!("{b}/issues?milestone={}&state=closed", m.number)) { (m.closed_items) " closed" }
                            }
                            @if can_write {
                                div.ms-actions {
                                    details.menu.row-menu {
                                        summary.btn.small { "Edit" }
                                        div.menu-pop.wide {
                                            form.form-tight method="post" action=(format!("{b}/milestones/{}/edit", m.number)) {
                                                label { "Title" input type="text" name="title" value=(m.title) required; }
                                                label { "Due date" input type="date" name="due" value=(m.due_ms.map(date_value).unwrap_or_default()); }
                                                label { "Description" textarea name="description" rows="3" { (m.description) } }
                                                div.form-actions { button.btn.small.primary type="submit" { "Save" } }
                                            }
                                        }
                                    }
                                    form method="post" action=(format!("{b}/milestones/{}/state", m.number)) {
                                        input type="hidden" name="state" value=(if m.state == "open" { "closed" } else { "open" });
                                        button.btn.small type="submit" { (if m.state == "open" { "Close" } else { "Reopen" }) }
                                    }
                                    form method="post" action=(format!("{b}/milestones/{}/delete", m.number)) data-confirm=(format!("Delete the milestone {:?}?", m.title)) {
                                        button.btn.small.danger type="submit" aria-label="Delete milestone" { (icon("trash")) }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    };
    let rev = default_rev(ctx);
    let n = nav(web, req, ctx, Tab::Issues, &rev).await;
    Ok(page(web, req, &format!("Milestones · {}", ctx.name), Some(n), body))
}

// ───────────── posts ─────────────

pub async fn post(web: &WebState, req: &Req, ctx: &RepoCtx, s: &[&str], f: &Fields) -> WebResult {
    let flux = &web.flux;
    let who = &req.principal;
    let b = base(ctx);
    match s {
        ["issues"] => {
            let triage = ctx.can(Role::Triage);
            let input = IssueInput {
                title: f.get("title").to_string(),
                body: f.get("body").replace("\r\n", "\n"),
                labels: if triage { f.all("labels") } else { vec![] },
                assignees: if triage { f.list("assignees") } else { vec![] },
                milestone: f.trim("milestone").parse().ok(),
            };
            match flux.open_issue(who, ctx.id, input).await {
                Ok(i) => Ok(to(&format!("{b}/issues/{}", i.number))),
                Err(e) => done(&format!("{b}/issues/new"), Err::<String, _>(e)),
            }
        }
        ["issues", n, action @ ..] => {
            let n = parse_num(n)?;
            let back = format!("{b}/issues/{n}");
            let edit = |e: IssueEdit| async move { flux.edit_issue(who, ctx.id, n, e).await.map(|_| ()) };
            let r: Result<String, WebError> = match action {
                ["comment"] => {
                    let text = f.get("body").replace("\r\n", "\n");
                    let mut msg = String::new();
                    if !text.trim().is_empty() {
                        flux.comment_issue(who, ctx.id, n, &text).await?;
                    } else if !f.has("state") {
                        return done(&back, Err::<String, _>(WebError::BadRequest("write a comment first".into())));
                    }
                    match f.trim("state") {
                        "closed" => {
                            let reason = match f.trim("reason") {
                                r @ ("not_planned" | "duplicate") => r,
                                _ => "completed",
                            };
                            edit(IssueEdit {
                                state: Some("closed".into()),
                                state_reason: Some(reason.into()),
                                ..Default::default()
                            })
                            .await
                            .map(|_| msg = "Issue closed".into())
                            .map_err(WebError::from)
                        }
                        "open" => edit(IssueEdit { state: Some("open".into()), ..Default::default() })
                            .await
                            .map(|_| msg = "Issue reopened".into())
                            .map_err(WebError::from),
                        _ => Ok(()),
                    }
                    .map(|_| msg)
                }
                ["edit"] => edit(IssueEdit {
                    title: Some(f.get("title").to_string()),
                    body: Some(f.get("body").replace("\r\n", "\n")),
                    ..Default::default()
                })
                .await
                .map(|_| "Issue updated".into())
                .map_err(Into::into),
                ["labels"] => edit(IssueEdit { labels: Some(f.all("labels")), ..Default::default() })
                    .await
                    .map(|_| "Labels updated".into())
                    .map_err(Into::into),
                ["assignees"] => edit(IssueEdit { assignees: Some(f.list("names")), ..Default::default() })
                    .await
                    .map(|_| "Assignees updated".into())
                    .map_err(Into::into),
                ["milestone"] => edit(IssueEdit { milestone: Some(f.trim("milestone").parse().ok()), ..Default::default() })
                    .await
                    .map(|_| "Milestone updated".into())
                    .map_err(Into::into),
                ["lock"] => {
                    let lock = f.trim("locked") == "1";
                    edit(IssueEdit { locked: Some(lock), ..Default::default() })
                        .await
                        .map(|_| if lock { "Conversation locked" } else { "Conversation unlocked" }.into())
                        .map_err(Into::into)
                }
                ["subscribe"] => {
                    let u = who.user.as_ref().ok_or_else(|| WebError::Unauthorized("sign in first".into()))?;
                    flux.authorize(who, ctx.id, Role::Read).await?;
                    let on = f.trim("state") == "subscribed";
                    flux.meta
                        .set_subscription(u.id, ctx.id, n, if on { Some("subscribed") } else { None })
                        .await
                        .map(|_| if on { "Subscribed" } else { "Unsubscribed" }.into())
                        .map_err(Into::into)
                }
                ["comments", seq, op @ ("edit" | "delete")] => {
                    let seq = parse_num(seq)?;
                    let body = f.get("body").replace("\r\n", "\n");
                    flux.change_comment(who, ctx.id, seq, (*op == "edit").then_some(body.as_str()))
                        .await
                        .map(|_| if *op == "edit" { "Comment updated" } else { "Comment deleted" }.into())
                        .map_err(Into::into)
                }
                _ => return Err(WebError::NotFound("no such action".into())),
            };
            done(&back, r)
        }
        ["labels"] => {
            let l =
                Label { name: f.trim("name").into(), color: f.trim("color").into(), description: f.trim("description").into() };
            done(&format!("{b}/labels"), flux.put_label(who, ctx.id, None, l).await.map(|_| "Label created".to_string()))
        }
        ["labels", "edit"] => {
            let l =
                Label { name: f.trim("name").into(), color: f.trim("color").into(), description: f.trim("description").into() };
            done(
                &format!("{b}/labels"),
                flux.put_label(who, ctx.id, Some(f.get("old")), l).await.map(|_| "Label saved".to_string()),
            )
        }
        ["labels", "delete"] => {
            done(&format!("{b}/labels"), flux.delete_label(who, ctx.id, f.get("name")).await.map(|_| "Label deleted".to_string()))
        }
        ["milestones"] => {
            let due = parse_date(f.get("due"));
            done(
                &format!("{b}/milestones"),
                flux.create_milestone(who, ctx.id, f.get("title"), f.trim("description"), due)
                    .await
                    .map(|m| format!("Created {}", m.title)),
            )
        }
        ["milestones", n, op @ ("edit" | "state" | "delete")] => {
            let n = parse_num(n)?;
            let to_list = format!("{b}/milestones");
            if *op == "delete" {
                return done(&to_list, flux.delete_milestone(who, ctx.id, n).await.map(|_| "Milestone deleted".to_string()));
            }
            let cur = flux
                .meta
                .list_milestones(ctx.id)
                .await?
                .into_iter()
                .find(|m| m.number == n)
                .ok_or_else(|| WebError::NotFound("milestone not found".into()))?;
            let r = if *op == "state" {
                let st = if f.trim("state") == "closed" { "closed" } else { "open" };
                flux.update_milestone(who, ctx.id, n, &cur.title, &cur.description, st, cur.due_ms)
                    .await
                    .map(|_| if st == "closed" { "Milestone closed" } else { "Milestone reopened" }.to_string())
            } else {
                let due = if f.trim("due").is_empty() { None } else { parse_date(f.get("due")) };
                flux.update_milestone(who, ctx.id, n, f.trim("title"), f.trim("description"), &cur.state, due)
                    .await
                    .map(|_| "Milestone saved".to_string())
            };
            done(&to_list, r)
        }
        _ => Err(WebError::NotFound("no such action".into())),
    }
}

/// Labels, reviewers, assignees and draft state of a review.
pub async fn review_sidebar_post(web: &WebState, req: &Req, ctx: &RepoCtx, n: i64, what: &str, f: &Fields) -> WebResult {
    let flux = &web.flux;
    let who = &req.principal;
    let back = format!("{}/reviews/{n}", base(ctx));
    let r: Result<String, WebError> = match what {
        "labels" => flux
            .apply_labels(who, ctx.id, n, &f.all("labels"), ctx.role)
            .await
            .map(|_| "Labels updated".into())
            .map_err(Into::into),
        "reviewers" => flux
            .set_people(who, ctx.id, n, "reviewer", &f.list("names"))
            .await
            .map(|_| "Reviewers updated".into())
            .map_err(Into::into),
        "assignees" => flux
            .set_people(who, ctx.id, n, "assignee", &f.list("names"))
            .await
            .map(|_| "Assignees updated".into())
            .map_err(Into::into),
        "ready" | "draft" => {
            let rv = flux.review(ctx.id, n).await?;
            let author = who.user.as_ref().is_some_and(|u| u.name == rv.author);
            if !(author || ctx.can(Role::Write)) {
                Err(WebError::Forbidden("only the author or writers can change the draft state".into()))
            } else if rv.state != "open" {
                Err(WebError::Conflict(format!("the review is {}", rv.state)))
            } else {
                flux.meta.set_review_draft(ctx.id, n, what == "draft").await?;
                if what == "ready" {
                    flux.review_event(ctx.id, n, "ready", who.name()).await;
                    Ok("Marked ready for review".into())
                } else {
                    Ok("Converted to a draft".into())
                }
            }
        }
        _ => Err(WebError::NotFound("no such action".into())),
    };
    done(&back, r)
}

/// Sidebar blocks for a review: reviewers (with their latest decision),
/// assignees and labels.
pub async fn review_side(web: &WebState, ctx: &RepoCtx, n: i64, decisions: &HashMap<String, &'static str>) -> WebResult<Markup> {
    let flux = &web.flux;
    let reviewers: Vec<String> = flux.meta.item_people(ctx.id, &[n], "reviewer").await?.into_iter().map(|(_, p)| p).collect();
    let assignees: Vec<String> = flux.meta.item_people(ctx.id, &[n], "assignee").await?.into_iter().map(|(_, p)| p).collect();
    let current: Vec<String> = flux.meta.item_labels(ctx.id, &[n]).await?.into_iter().map(|(_, l)| l).collect();
    let (labels, colors) = label_colors(web, ctx).await?;
    let triage = ctx.can(Role::Triage);
    let b = format!("{}/reviews/{n}", base(ctx));
    // Everyone who reviewed, requested or not.
    let mut people: Vec<String> = reviewers.clone();
    for k in decisions.keys() {
        if !people.contains(k) {
            people.push(k.clone());
        }
    }
    people.sort();
    Ok(html! {
        div.side-block {
            div.side-title.row { "Reviewers" div.spacer {}
                @if triage {
                    details.side-edit {
                        summary.link.small { "Request" }
                        form.side-form method="post" action=(format!("{b}/reviewers")) {
                            input type="text" name="names" value=(reviewers.join(", ")) placeholder="alice, bob" aria-label="Reviewers";
                            button.btn.small type="submit" { "Save" }
                        }
                    }
                }
            }
            @if people.is_empty() { span.muted { "No reviews yet" } }
            @for p in &people {
                div.person {
                    (avatar(p, "", false)) a href=(view::repo_url(p)) { (p) }
                    div.spacer {}
                    @match decisions.get(p).copied() {
                        Some("approved") => { span.decision.ok title="Approved" { (icon("check")) } }
                        Some("changes_requested") => { span.decision.bad title="Requested changes" { (icon("x")) } }
                        _ => { span.decision.wait title="Awaiting review" { (view::sdot("pending")) } }
                    }
                }
            }
        }
        (side_people("Assignees", "assignees", &b, &assignees, triage, "No one assigned"))
        div.side-block {
            div.side-title.row { "Labels" div.spacer {} @if triage { (labels_editor(&b, &labels, &current)) } }
            @if current.is_empty() { span.muted { "None yet" } }
            div.label-list { @for l in &current { (pill(&colors, l)) } }
        }
    })
}

/// Label pills for many items at once (review lists).
pub async fn labels_for(web: &WebState, ctx: &RepoCtx, numbers: &[i64]) -> WebResult<HashMap<i64, Vec<Markup>>> {
    if numbers.is_empty() {
        return Ok(HashMap::new());
    }
    let (_, colors) = label_colors(web, ctx).await?;
    let mut out: HashMap<i64, Vec<Markup>> = HashMap::new();
    for (n, l) in web.flux.meta.item_labels(ctx.id, numbers).await? {
        out.entry(n).or_default().push(pill(&colors, &l));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    #[test]
    fn dates() {
        assert_eq!(super::parse_date("1970-01-02"), Some(86_400_000));
        assert_eq!(super::date_value(super::parse_date("2024-02-29").unwrap()), "2024-02-29");
        assert_eq!(super::parse_date("nope"), None);
    }
}
