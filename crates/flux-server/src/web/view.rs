//! Layout, components and formatting helpers for the web UI.

use std::time::{SystemTime, UNIX_EPOCH};

use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Response};
use flux_core::{ChangesetId, RepoPath};
use maud::{DOCTYPE, Markup, PreEscaped, html};

use super::{Req, Web, security_headers};

pub const FAVICON: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 32 32"><rect width="32" height="32" rx="8" fill="#6d5dfc"/><path d="M9 22c4-1 5-4 7-8s4-6 7-6" stroke="#fff" stroke-width="3" fill="none" stroke-linecap="round"/><circle cx="9" cy="22" r="2.5" fill="#fff"/><circle cx="23" cy="8" r="2.5" fill="#fff"/></svg>"##;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Code,
    Commits,
    Reviews,
    Branches,
    Activity,
    Search,
}

pub struct RepoNav<'a> {
    pub repo: &'a str,
    pub tab: Tab,
    /// Revision for code links (branch name or changeset id).
    pub rev: &'a str,
    pub open_reviews: usize,
}

// ───────────── icons (inline SVG, 16px) ─────────────

pub fn icon(name: &str) -> Markup {
    let d = match name {
        "dir" => "M1.75 2.5h4.1l1.6 1.6h6.8c.4 0 .75.35.75.75v7.4c0 .4-.35.75-.75.75H1.75A.75.75 0 0 1 1 12.25v-9c0-.4.35-.75.75-.75Z",
        "file" => "M3.75 1.5h5.3L13 5.45v8.8c0 .4-.35.75-.75.75h-8.5A.75.75 0 0 1 3 14.25V2.25c0-.4.35-.75.75-.75Zm5 .75v3.5h3.5",
        "link" => "M7 9.5a3 3 0 0 0 4.2 0l2-2a3 3 0 0 0-4.2-4.2l-.6.6M9 6.5a3 3 0 0 0-4.2 0l-2 2a3 3 0 0 0 4.2 4.2l.6-.6",
        "module" => "M2 4.5 8 1.5l6 3v7l-6 3-6-3Zm0 0 6 3 6-3M8 7.5v7",
        "commit" => "M8 5.25a2.75 2.75 0 1 0 0 5.5 2.75 2.75 0 0 0 0-5.5ZM1 8h4.25m5.5 0H15",
        "branch" => "M4.5 2.5v11M4.5 9.5c0-3 7-2 7-5M11.5 2.25a1.25 1.25 0 1 1 0 2.5 1.25 1.25 0 0 1 0-2.5ZM4.5 13.5a1.25 1.25 0 1 1 0-2.5",
        "review" => "M2.5 2.5h11v8h-6l-3 3v-3h-2Z",
        "search" => "M7 2a5 5 0 1 0 0 10A5 5 0 0 0 7 2Zm3.6 8.6L14 14",
        "clock" => "M8 1.75a6.25 6.25 0 1 0 0 12.5 6.25 6.25 0 0 0 0-12.5ZM8 4.5V8l2.5 1.5",
        "code" => "M5.5 4 1.5 8l4 4M10.5 4l4 4-4 4",
        "pulse" => "M1 8h3l2-5 4 10 2-5h3",
        "copy" => "M5.5 5.5V2.25c0-.4.35-.75.75-.75h7.5c.4 0 .75.35.75.75v7.5c0 .4-.35.75-.75.75H10.5M2.25 5.5h7.5c.4 0 .75.35.75.75v7.5c0 .4-.35.75-.75.75h-7.5a.75.75 0 0 1-.75-.75v-7.5c0-.4.35-.75.75-.75Z",
        "check" => "M2.5 8.5 6 12l7.5-8",
        "x" => "M4 4l8 8M12 4l-8 8",
        "merge" => "M4.5 2.5v11M4.5 6c3 0 7 1 7 4.5v3M4.5 2.25a1.25 1.25 0 1 1 0 2.5M11.5 13.75a1.25 1.25 0 1 1 0-2.5",
        "sun" => "M8 5a3 3 0 1 0 0 6 3 3 0 0 0 0-6ZM8 1v1.5M8 13.5V15M1 8h1.5M13.5 8H15M3 3l1 1M12 12l1 1M3 13l1-1M12 4l1-1",
        "history" => "M1.75 8a6.25 6.25 0 1 0 1.8-4.4M1.5 1.75v3h3M8 4.5V8l2.5 1.5",
        "raw" => "M4 2.5h8M4 6h8M4 9.5h5M4 13h3",
        "blame" => "M2 3h3M2 7h3M2 11h3M7 3h7M7 7h5M7 11h7",
        "lock" => "M4.5 7V5a3.5 3.5 0 0 1 7 0v2M3.5 7h9v7h-9Z",
        _ => "M2 2h12v12H2Z",
    };
    html! {
        svg.icon viewBox="0 0 16 16" width="16" height="16" aria-hidden="true" {
            path d=(d) fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" {}
        }
    }
}

// ───────────── formatting ─────────────

pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// Civil date from days since the epoch (Howard Hinnant's algorithm).
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

pub fn iso(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let (y, m, d) = civil(secs.div_euclid(86_400));
    let t = secs.rem_euclid(86_400);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", t / 3600, (t / 60) % 60, t % 60)
}

pub fn day_label(ms: i64) -> String {
    let (y, m, d) = civil(ms.div_euclid(1000).div_euclid(86_400));
    format!("{} {d}, {y}", MONTHS[(m - 1) as usize])
}

pub fn day_key(ms: i64) -> i64 {
    ms.div_euclid(1000).div_euclid(86_400)
}

pub fn ago(ms: i64) -> String {
    let s = (now_ms() - ms).max(0) / 1000;
    match s {
        0..=44 => "just now".into(),
        45..=89 => "a minute ago".into(),
        90..=3_299 => format!("{} minutes ago", (s + 30) / 60),
        3_300..=5_399 => "an hour ago".into(),
        5_400..=79_199 => format!("{} hours ago", (s + 1800) / 3600),
        79_200..=129_599 => "yesterday".into(),
        129_600..=2_591_999 => format!("{} days ago", (s + 43_200) / 86_400),
        2_592_000..=31_103_999 => {
            let n = (s + 1_296_000) / 2_592_000;
            if n == 1 { "last month".into() } else { format!("{n} months ago") }
        }
        _ => {
            let n = s / 31_536_000;
            if n <= 1 { "last year".into() } else { format!("{n} years ago") }
        }
    }
}

pub fn time(ms: i64) -> Markup {
    html! { time datetime=(iso(ms)) title=(iso(ms).replace('T', " ")) { (ago(ms)) } }
}

pub fn size(bytes: u64) -> String {
    match bytes {
        0..1024 => format!("{bytes} B"),
        1024..1_048_576 => format!("{:.1} KB", bytes as f64 / 1024.0),
        1_048_576..1_073_741_824 => format!("{:.1} MB", bytes as f64 / 1_048_576.0),
        _ => format!("{:.2} GB", bytes as f64 / 1_073_741_824.0),
    }
}

pub fn count(n: u64) -> String {
    match n {
        0..10_000 => n.to_string(),
        10_000..1_000_000 => format!("{:.1}k", n as f64 / 1000.0),
        _ => format!("{:.1}M", n as f64 / 1_000_000.0),
    }
}

/// `"Name <email>"` → (name, email).
pub fn person(raw: &[u8]) -> (String, String) {
    let s = String::from_utf8_lossy(raw);
    match (s.find('<'), s.rfind('>')) {
        (Some(a), Some(b)) if a < b => (s[..a].trim().to_string(), s[a + 1..b].trim().to_string()),
        _ => (s.trim().to_string(), String::new()),
    }
}

pub fn avatar(name: &str, email: &str, big: bool) -> Markup {
    let initials: String = name
        .split_whitespace()
        .filter_map(|w| w.chars().next())
        .take(2)
        .collect::<String>()
        .to_uppercase();
    let initials = if initials.is_empty() { "?".into() } else { initials };
    let key = if email.is_empty() { name } else { email };
    // 24 hue buckets, defined in CSS (CSP forbids inline styles).
    let hue = blake3::hash(key.to_ascii_lowercase().as_bytes()).as_bytes()[0] as u32 % 24;
    let class = if big { format!("avatar big h{hue}") } else { format!("avatar h{hue}") };
    html! { span class=(class) title=(name) { (initials) } }
}

pub fn subject(msg: &[u8]) -> String {
    let s = String::from_utf8_lossy(msg);
    s.lines().find(|l| !l.trim().is_empty()).unwrap_or("(no message)").trim().to_string()
}

pub fn body_text(msg: &[u8]) -> String {
    let s = String::from_utf8_lossy(msg);
    let mut lines = s.lines().skip_while(|l| l.trim().is_empty());
    lines.next();
    lines.collect::<Vec<_>>().join("\n").trim().to_string()
}

pub fn short(id: &ChangesetId) -> String {
    id.to_hex()[..10].to_string()
}

/// Percent-encode a path for URLs (keeping `/`).
pub fn enc_path(p: &[u8]) -> String {
    let mut out = String::with_capacity(p.len());
    for &b in p {
        if b.is_ascii_alphanumeric() || b"/-_.~!$&'()*+,;=:@".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

pub fn enc(s: &str) -> String {
    enc_path(s.as_bytes())
}

pub fn url(repo: &str, action: &str, rev: &str, path: Option<&RepoPath>) -> String {
    match path {
        Some(p) => format!("/{}/-/{action}/{}/{}", enc(repo), enc(rev), enc_path(p.as_bytes())),
        None => format!("/{}/-/{action}/{}", enc(repo), enc(rev)),
    }
}

pub fn repo_url(repo: &str) -> String {
    format!("/{}", enc(repo))
}

// ───────────── layout ─────────────

pub fn page(web: &Web, req: &Req, title: &str, nav: Option<RepoNav<'_>>, body: Markup) -> Response {
    let user = req.principal.user.as_ref().map(|u| u.name.clone());
    let mode = req.principal.mode;
    let markup = html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                meta name="color-scheme" content="light dark";
                title { (title) " · flux" }
                link rel="icon" href="/-/static/favicon.svg" type="image/svg+xml";
                link rel="stylesheet" href=(web.css_path);
                script src=(web.js_path) defer {}
            }
            body {
                header.top {
                    div.top-inner {
                        a.brand href="/" { (PreEscaped(FAVICON)) span { "flux" } }
                        @if let Some(n) = &nav {
                            span.sep { "/" }
                            a.repo-name href=(repo_url(n.repo)) { (n.repo) }
                        }
                        div.spacer {}
                        @if let Some(n) = &nav {
                            form.top-search action=(format!("/{}/-/search", enc(n.repo))) method="get" {
                                (icon("search"))
                                input type="search" name="q" placeholder="Search code…  ( / )" aria-label="Search code" autocomplete="off";
                                input type="hidden" name="rev" value=(n.rev);
                            }
                            button.btn.ghost.finder-btn type="button" data-finder=(format!("/{}/-/files/{}", enc(n.repo), enc(n.rev))) title="Go to file (t)" {
                                (icon("file")) span.hide-sm { "Go to file" } kbd { "t" }
                            }
                        }
                        button.btn.ghost.theme-toggle type="button" title="Toggle theme" aria-label="Toggle theme" { (icon("sun")) }
                        @match (&user, mode) {
                            (Some(u), _) => {
                                form.inline action="/-/logout" method="post" {
                                    span.user { (avatar(u, "", false)) span.hide-sm { (u) } }
                                    button.btn.ghost type="submit" { "Sign out" }
                                }
                            }
                            (None, crate::auth::AuthMode::Open) => {}
                            (None, _) => { a.btn href=(format!("/-/login?next={}", enc(&req.path))) { "Sign in" } }
                        }
                    }
                }
                @if let Some(n) = &nav {
                    nav.repo-tabs {
                        div.tabs-inner {
                            (tab(n, Tab::Code, "code", "Code", &format!("/{}/-/tree/{}", enc(n.repo), enc(n.rev))))
                            (tab(n, Tab::Commits, "history", "Changes", &format!("/{}/-/commits/{}", enc(n.repo), enc(n.rev))))
                            (tab(n, Tab::Reviews, "review", &format!("Reviews{}", if n.open_reviews > 0 { format!(" · {}", n.open_reviews) } else { String::new() }), &format!("/{}/-/reviews", enc(n.repo))))
                            (tab(n, Tab::Branches, "branch", "Branches", &format!("/{}/-/branches", enc(n.repo))))
                            (tab(n, Tab::Activity, "pulse", "Activity", &format!("/{}/-/activity", enc(n.repo))))
                            (tab(n, Tab::Search, "search", "Search", &format!("/{}/-/search?rev={}", enc(n.repo), enc(n.rev))))
                        }
                    }
                }
                main #main data-repo=[nav.as_ref().map(|n| n.repo)] data-rev=[nav.as_ref().map(|n| n.rev)] {
                    div #live-banner hidden {}
                    (body)
                }
                footer.foot {
                    span { "flux " (env!("CARGO_PKG_VERSION")) }
                    span.dot { "·" }
                    span #render-time { "rendered in " (format!("{:.2}", req.started.elapsed().as_secs_f64() * 1000.0)) " ms" }
                    span.dot { "·" }
                    a href="https://github.com/virajbhartiya/flux" { "source" }
                }
                div #finder hidden {
                    div.finder-box {
                        input #finder-input type="text" placeholder="Go to file…" autocomplete="off" spellcheck="false";
                        ul #finder-list {}
                        div.finder-hint { kbd { "↑" } kbd { "↓" } " navigate " kbd { "↵" } " open " kbd { "esc" } " close" }
                    }
                }
            }
        }
    };
    html_response(req, markup)
}

fn tab(n: &RepoNav<'_>, which: Tab, icon_name: &str, label: &str, href: &str) -> Markup {
    html! {
        a.tab.active[n.tab == which] href=(href) { (icon(icon_name)) span { (label) } }
    }
}

pub fn html_response(req: &Req, markup: Markup) -> Response {
    let mut r = markup.into_string().into_response();
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("private, no-cache"));
    h.insert(header::VARY, HeaderValue::from_static("Cookie, X-Flux-Partial"));
    if let Ok(v) = HeaderValue::from_str(&format!("app;dur={:.3}", req.started.elapsed().as_secs_f64() * 1000.0)) {
        h.insert("server-timing", v);
    }
    security_headers(h);
    r
}

pub fn error_page(status: u16, title: &str, msg: &str) -> String {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) " · flux" }
                link rel="icon" href="/-/static/favicon.svg" type="image/svg+xml";
                style { (PreEscaped(ERROR_CSS)) }
            }
            body {
                main #main.error {
                    div.code { (status) }
                    h1 { (title) }
                    p { (msg) }
                    p { a href="/" { "← All repositories" } }
                }
            }
        }
    }
    .into_string()
}

const ERROR_CSS: &str = "body{font:15px/1.5 system-ui,sans-serif;margin:0;display:grid;place-items:center;min-height:100vh;background:#fafafa;color:#1f2328}main{max-width:560px;padding:32px;text-align:center}.code{font-size:64px;font-weight:700;color:#6d5dfc}h1{margin:0 0 8px}p{color:#59636e;word-break:break-word}a{color:#6d5dfc}@media(prefers-color-scheme:dark){body{background:#0d1117;color:#e6edf3}p{color:#9198a1}}";

pub fn breadcrumbs(repo: &str, rev: &str, path: Option<&RepoPath>) -> Markup {
    let comps: Vec<&[u8]> = path.map(|p| p.components().collect()).unwrap_or_default();
    html! {
        nav.crumbs {
            a href=(url(repo, "tree", rev, None)) { strong { (repo.rsplit('/').next().unwrap_or(repo)) } }
            @for (i, c) in comps.iter().enumerate() {
                span.sep { "/" }
                @if i + 1 == comps.len() {
                    strong { (String::from_utf8_lossy(c)) }
                } @else {
                    @let sub = RepoPath::from_components(comps[..=i].iter().copied()).ok();
                    a href=(url(repo, "tree", rev, sub.as_ref())) { (String::from_utf8_lossy(c)) }
                }
            }
            @if let Some(p) = path {
                button.copy-btn type="button" data-copy=(p.to_string()) title="Copy path" { (icon("copy")) }
            }
        }
    }
}

pub fn empty_state(title: &str, text: Markup) -> Markup {
    html! {
        div.empty {
            div.empty-art { (PreEscaped(FAVICON)) }
            h2 { (title) }
            div.empty-text { (text) }
        }
    }
}
