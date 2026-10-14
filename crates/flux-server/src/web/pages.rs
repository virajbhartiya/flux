//! Page handlers.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use axum::extract::{Form, Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Redirect, Response};
use flux_core::{ChangesetId, DerivedId, EntryKind, FileKind, RepoPath, TreeEntry, TreeId};
use flux_derive::{LASTMOD_V1, LmNode, diff_trees, lastmod_dir, lookup_path};
use flux_store::RepoId;
use maud::{Markup, PreEscaped, html};
use serde::Deserialize;

use super::diff::{FileDiff, LineKind, diff_text, is_binary};
use super::markdown::{self, LinkBase};
use super::view::{
    self, RepoNav, Tab, avatar, breadcrumbs, empty_state, enc, enc_path, icon, page, person, short, subject, time, url,
};
use super::{COOKIE, Req, WebError, WebResult, WebState};
use crate::core::{Flux, Loaded};

const PAGE_SIZE: usize = 50;
/// Diff lines rendered per page before files are collapsed.
const DIFF_LINE_BUDGET: usize = 12_000;
const MAX_DIFF_FILE: u64 = 1 << 20;
/// Lines shown in the file viewer before truncating.
const MAX_VIEW_LINES: usize = 25_000;

fn query_map(q: &str) -> HashMap<String, String> {
    url_decode_pairs(q).into_iter().collect()
}

/// Percent-decode (query strings: `+` is a space).
fn url_decode(s: &str) -> String {
    String::from_utf8_lossy(&percent_decode(s, true)).into_owned()
}

/// Percent-decode a path segment (`+` is literal).
fn url_decode_bytes(s: &str) -> Vec<u8> {
    percent_decode(s, false)
}

fn percent_decode(s: &str, plus_is_space: bool) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' if plus_is_space => out.push(b' '),
            b'%' if i + 3 <= b.len() => {
                match std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(v) => {
                        out.push(v);
                        i += 2;
                    }
                    None => out.push(b'%'),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    out
}

fn url_decode_pairs(q: &str) -> Vec<(String, String)> {
    q.split('&')
        .filter(|kv| !kv.is_empty())
        .map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            (url_decode(k), url_decode(v))
        })
        .collect()
}

// ───────────── shared lookups ─────────────

struct RepoCtx {
    name: String,
    id: RepoId,
    refs: Arc<Vec<(String, ChangesetId)>>,
}

impl RepoCtx {
    fn default_branch(&self) -> Option<&str> {
        ["main", "master", "trunk"]
            .iter()
            .find(|b| self.refs.iter().any(|(n, _)| n == *b))
            .copied()
            .or_else(|| self.refs.first().map(|r| r.0.as_str()))
    }

    fn is_ref(&self, name: &str) -> bool {
        self.refs.iter().any(|(n, _)| n == name)
    }

    /// Split `rev/path...` where the rev may itself contain slashes: the
    /// longest prefix naming a ref wins, otherwise the first segment.
    fn split_rev<'a>(&self, segs: &'a [String]) -> (String, &'a [String]) {
        for k in (1..=segs.len()).rev() {
            let cand = segs[..k].join("/");
            if self.is_ref(&cand) {
                return (cand, &segs[k..]);
            }
        }
        match segs.split_first() {
            Some((first, rest)) => (first.clone(), rest),
            None => (self.default_branch().unwrap_or("main").to_string(), &[]),
        }
    }
}

async fn repo_ctx(web: &WebState, req: &Req, name: &str) -> WebResult<RepoCtx> {
    req.principal.require_read()?;
    let id = web.flux.repo(name).await?;
    let refs = match web.refs.get(&id) {
        Some(r) => r,
        None => {
            let list = web.flux.meta.list_refs(id, b"", 10_000).await?;
            let r: Arc<Vec<(String, ChangesetId)>> =
                Arc::new(list.into_iter().map(|(n, v)| (String::from_utf8_lossy(&n).into_owned(), v.target)).collect());
            web.refs.insert(id, r.clone());
            r
        }
    };
    Ok(RepoCtx { name: name.to_string(), id, refs })
}

async fn open_reviews(flux: &Flux, repo: RepoId) -> usize {
    flux.meta.list_reviews(repo, Some("open"), 1000).await.map(|v| v.len()).unwrap_or(0)
}

fn path_from(segs: &[String]) -> WebResult<Option<RepoPath>> {
    let joined: Vec<u8> = segs.iter().map(|s| url_decode_bytes(s)).collect::<Vec<_>>().join(&b'/');
    if joined.is_empty() {
        return Ok(None);
    }
    RepoPath::new(joined).map(Some).map_err(|e| WebError::BadRequest(e.to_string()))
}

/// The lastmod root if already derived; otherwise kick off derivation in
/// the background and return `None` (pages render without it).
async fn lastmod_ready(flux: &Arc<Flux>, repo: RepoId, cs: &ChangesetId) -> Option<DerivedId> {
    match flux.derived_get(repo, LASTMOD_V1, cs).await.ok()? {
        Some(v) => Some(DerivedId::from_bytes(v)),
        None => {
            let (f, c) = (flux.clone(), *cs);
            tokio::spawn(async move {
                let _ = f.lastmod(repo, &c).await;
            });
            None
        }
    }
}

fn rev_label(rev: &str) -> String {
    if rev.len() == 64 && rev.bytes().all(|b| b.is_ascii_hexdigit()) { rev[..10].to_string() } else { rev.to_string() }
}

// ───────────── top-level pages ─────────────

pub async fn home(State(web): State<WebState>, req: Req) -> WebResult {
    req.principal.require_read()?;
    let repos = web.flux.meta.list_repos().await?;
    let mut cards = Vec::new();
    for r in &repos {
        let refs = web.flux.meta.list_refs(r.id, b"", 1000).await?;
        let head = refs
            .iter()
            .find(|(n, _)| n == b"main")
            .or_else(|| refs.first())
            .map(|(n, v)| (String::from_utf8_lossy(n).into_owned(), v.target));
        let latest = match &head {
            Some((_, id)) => web.flux.load_one_arc(r.id, id).await.ok(),
            None => None,
        };
        cards.push((r.name.clone(), refs.len(), head, latest));
    }
    cards.sort_by_key(|c| std::cmp::Reverse(c.3.as_ref().map(|l| l.changeset.author_time.unix_ms).unwrap_or(0)));
    let body = html! {
        div.container {
            div.page-head {
                h1 { "Repositories" }
                p.muted { (repos.len()) " repositories on this server" }
            }
            @if cards.is_empty() {
                (empty_state("No repositories yet", html! {
                    p { "Create one with the CLI, then import history or start committing:" }
                    pre.cmd { "flux repo create demo --server http://HOST\nflux-import-git --repo demo import /path/to/repo.git --create" }
                }))
            }
            div.repo-grid {
                @for (name, nrefs, head, latest) in &cards {
                    a.repo-card href=(view::repo_url(name)) {
                        div.repo-card-title { (icon("code")) strong { (name) } }
                        @if let Some(l) = latest {
                            div.repo-card-msg { (subject(&l.changeset.message)) }
                            div.repo-card-meta {
                                @let (n, e) = person(&l.changeset.author);
                                (avatar(&n, &e, false)) span { (n) } span.dot { "·" } (time(l.changeset.author_time.unix_ms))
                            }
                        } @else {
                            div.repo-card-msg.muted { "Empty repository" }
                        }
                        div.repo-card-foot {
                            span { (icon("branch")) (nrefs) " branches" }
                            @if let Some((b, _)) = head { span.pill { (b) } }
                        }
                    }
                }
            }
        }
    };
    Ok(page(&web, &req, "Repositories", None, body))
}

pub async fn readyz(State(web): State<WebState>) -> Response {
    match web.flux.meta.list_repos().await {
        Ok(_) => "ready".into_response(),
        Err(e) => (StatusCode::SERVICE_UNAVAILABLE, format!("metadata store unavailable: {e}")).into_response(),
    }
}

pub async fn metrics(State(web): State<WebState>) -> Response {
    let mut out = crate::metrics::render();
    let b = web.flux.blobs.stats();
    out.push_str(&format!(
        "# TYPE flux_blob_objects gauge\nflux_blob_objects {}\n# TYPE flux_blob_bytes gauge\nflux_blob_bytes {}\n# TYPE flux_uptime_seconds gauge\nflux_uptime_seconds {}\n",
        b.objects,
        b.bytes,
        web.started.elapsed().as_secs()
    ));
    ([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], out).into_response()
}

#[derive(Deserialize)]
pub struct LoginForm {
    token: String,
    #[serde(default)]
    next: String,
}

pub async fn login_form(State(web): State<WebState>, req: Req) -> WebResult {
    let next = query_map(&req.query).remove("next").unwrap_or_else(|| "/".into());
    let body = html! {
        div.container.narrow {
            div.card.login {
                h1 { "Sign in to flux" }
                p.muted { "Paste an access token. An admin can create one with " code { "flux-server token --user NAME" } "." }
                form method="post" action="/-/login" {
                    input type="hidden" name="next" value=(next);
                    label { "Token" input type="password" name="token" placeholder="flx_…" autofocus required autocomplete="current-password"; }
                    button.btn.primary type="submit" { "Sign in" }
                }
            }
        }
    };
    Ok(page(&web, &req, "Sign in", None, body))
}

fn safe_next(next: &str) -> &str {
    if next.starts_with('/') && !next.starts_with("//") && !next.contains('\\') { next } else { "/" }
}

pub async fn login(State(web): State<WebState>, headers: HeaderMap, Form(f): Form<LoginForm>) -> WebResult {
    check_origin(&headers)?;
    let p = web.flux.principal(Some(&f.token)).await?;
    if p.user.is_none() {
        return Err(WebError::Unauthorized("invalid token".into()));
    }
    let secure = headers.get("x-forwarded-proto").and_then(|v| v.to_str().ok()) == Some("https");
    let cookie = format!(
        "{COOKIE}={}; Path=/; HttpOnly; SameSite=Lax; Max-Age=2592000{}",
        f.token.trim(),
        if secure { "; Secure" } else { "" }
    );
    let mut r = Redirect::to(safe_next(&f.next)).into_response();
    r.headers_mut().insert(header::SET_COOKIE, HeaderValue::from_str(&cookie).map_err(|e| WebError::BadRequest(e.to_string()))?);
    Ok(r)
}

pub async fn logout(headers: HeaderMap) -> WebResult {
    check_origin(&headers)?;
    let mut r = Redirect::to("/").into_response();
    r.headers_mut()
        .insert(header::SET_COOKIE, HeaderValue::from_static("flux_token=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0"));
    Ok(r)
}

/// CSRF defence in depth (cookies are SameSite=Lax): POSTs must come from
/// this origin when the browser says where they came from.
fn check_origin(headers: &HeaderMap) -> WebResult<()> {
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

// ───────────── repo dispatch ─────────────

fn split_repo(rest: &str) -> (String, Vec<String>) {
    let rest = rest.trim_matches('/');
    match rest.split_once("/-/") {
        Some((repo, action)) => (url_decode(repo), action.split('/').filter(|s| !s.is_empty()).map(str::to_string).collect()),
        None => match rest.strip_suffix("/-") {
            Some(repo) => (url_decode(repo), vec![]),
            None => (url_decode(rest), vec![]),
        },
    }
}

pub async fn repo_get(State(web): State<WebState>, req: Req, Path(rest): Path<String>) -> WebResult {
    let (repo, segs) = split_repo(&rest);
    let ctx = repo_ctx(&web, &req, &repo).await?;
    let Some((action, args)) = segs.split_first() else {
        let rev = ctx.default_branch().map(str::to_string);
        return match rev {
            Some(r) => tree(&web, &req, &ctx, &r, None).await,
            None => empty_repo(&web, &req, &ctx).await,
        };
    };
    let q = query_map(&req.query);
    match action.as_str() {
        "tree" => {
            let (rev, rest) = ctx.split_rev(args);
            tree(&web, &req, &ctx, &rev, path_from(rest)?).await
        }
        "blob" => {
            let (rev, rest) = ctx.split_rev(args);
            let path = path_from(rest)?.ok_or_else(|| WebError::BadRequest("missing path".into()))?;
            blob(&web, &req, &ctx, &rev, &path, q.contains_key("plain")).await
        }
        "raw" => {
            let (rev, rest) = ctx.split_rev(args);
            let path = path_from(rest)?.ok_or_else(|| WebError::BadRequest("missing path".into()))?;
            raw(&web, &ctx, &rev, &path).await
        }
        "blame" => {
            let (rev, rest) = ctx.split_rev(args);
            let path = path_from(rest)?.ok_or_else(|| WebError::BadRequest("missing path".into()))?;
            blame(&web, &req, &ctx, &rev, &path).await
        }
        "commits" => {
            let (rev, rest) = ctx.split_rev(args);
            let page_no: usize = q.get("page").and_then(|p| p.parse().ok()).unwrap_or(0);
            commits(&web, &req, &ctx, &rev, path_from(rest)?, page_no).await
        }
        "commit" => {
            let rev = args.join("/");
            commit(&web, &req, &ctx, &rev).await
        }
        "compare" => compare(&web, &req, &ctx, &url_decode(&args.join("/"))).await,
        "branches" => branches(&web, &req, &ctx).await,
        "activity" => activity(&web, &req, &ctx, q.get("before").and_then(|b| b.parse().ok())).await,
        "search" => search(&web, &req, &ctx, &q).await,
        "files" => {
            let (rev, _) = ctx.split_rev(args);
            files_json(&web, &ctx, &rev).await
        }
        "events" => Ok(events(&web, &ctx)),
        "reviews" => match args {
            [] => reviews(&web, &req, &ctx, q.get("state").map(String::as_str).unwrap_or("open")).await,
            [n] if n == "new" => new_review(&web, &req, &ctx, &q).await,
            [n] => review(&web, &req, &ctx, parse_num(n)?, false).await,
            [n, t] if t == "files" => review(&web, &req, &ctx, parse_num(n)?, true).await,
            _ => Err(WebError::NotFound("no such page".into())),
        },
        _ => Err(WebError::NotFound(format!("unknown page {action:?}"))),
    }
}

fn parse_num(s: &str) -> WebResult<i64> {
    s.parse().map_err(|_| WebError::NotFound(format!("no review {s:?}")))
}

async fn nav<'a>(web: &WebState, ctx: &'a RepoCtx, tab: Tab, rev: &'a str) -> RepoNav<'a> {
    RepoNav { repo: &ctx.name, tab, rev, open_reviews: open_reviews(&web.flux, ctx.id).await }
}

async fn empty_repo(web: &WebState, req: &Req, ctx: &RepoCtx) -> WebResult {
    let host = "http://HOST:7878";
    let body = html! {
        div.container {
            (empty_state("This repository is empty", html! {
                p { "Start a working copy and make the first commit:" }
                pre.cmd { "flux attach " (host) " " (ctx.name) " " (ctx.name.rsplit('/').next().unwrap_or(&ctx.name)) "\ncd " (ctx.name.rsplit('/').next().unwrap_or(&ctx.name)) "\necho hello > README.md\nflux commit -A -m \"First commit\"\nflux set-ref main @" }
                p { "Or import an existing git repository:" }
                pre.cmd { "flux-import-git --server " (host) " --repo " (ctx.name) " import /path/to/repo.git" }
            }))
        }
    };
    let n = nav(web, ctx, Tab::Code, "main").await;
    Ok(page(web, req, &ctx.name, Some(n), body))
}

// ───────────── code ─────────────

fn kind_icon(e: &TreeEntry) -> &'static str {
    match e.kind {
        EntryKind::Dir => "dir",
        EntryKind::File(FileKind::Symlink) => "link",
        EntryKind::File(FileKind::GitLink(_)) => "module",
        EntryKind::File(_) => "file",
    }
}

fn ref_picker(ctx: &RepoCtx, rev: &str, action: &str, path: Option<&RepoPath>) -> Markup {
    let label = rev_label(rev);
    html! {
        details.picker {
            summary.btn { (icon("branch")) span.picker-label { (label) } span.caret {} }
            div.picker-menu {
                input.picker-filter type="text" placeholder="Find a branch…" autocomplete="off";
                ul {
                    @for (name, _) in ctx.refs.iter().take(500) {
                        li { a.active[name == rev] href=(url(&ctx.name, action, name, path)) { (name) } }
                    }
                }
            }
        }
    }
}

fn commit_bar(ctx: &RepoCtx, l: &Loaded, rev: &str, history: &str) -> Markup {
    let (n, e) = person(&l.changeset.author);
    html! {
        div.commit-bar {
            (avatar(&n, &e, false))
            strong.who { (n) }
            a.msg href=(url(&ctx.name, "commit", &l.id.to_hex(), None)) { (subject(&l.changeset.message)) }
            div.spacer {}
            a.sha href=(url(&ctx.name, "commit", &l.id.to_hex(), None)) { (short(&l.id)) }
            span.dot { "·" }
            (time(l.changeset.author_time.unix_ms))
            a.btn.small href=(history) title="History" { (icon("history")) span.hide-sm { "History" } }
            span.hidden { (rev) }
        }
    }
}

async fn tree(web: &WebState, req: &Req, ctx: &RepoCtx, rev: &str, path: Option<RepoPath>) -> WebResult {
    let flux = &web.flux;
    let cs = flux.resolve_rev(ctx.id, rev).await?;
    let root = flux.tree_of(ctx.id, &cs).await?;
    let dir: TreeId = match &path {
        None => root,
        Some(p) => match lookup_path(&flux.blobs, root, p).await? {
            Some(e) if e.is_dir() => e.tree_id().unwrap(),
            Some(_) => return Ok(Redirect::to(&url(&ctx.name, "blob", rev, Some(p))).into_response()),
            None => return Err(WebError::NotFound(format!("{p} does not exist at {}", rev_label(rev)))),
        },
    };
    let t = flux.blobs.get_tree(&dir).await?.ok_or_else(|| WebError::Internal("missing tree".into()))?;
    let lm_root = lastmod_ready(flux, ctx.id, &cs).await;
    let lm: Option<Arc<LmNode>> = match lm_root {
        Some(r) => lastmod_dir(&flux.blobs, r, path.as_ref()).await?,
        None => None,
    };
    // Latest change to this directory.
    let latest_id = match (&path, lm_root) {
        (None, _) => Some(cs),
        (Some(p), Some(r)) => flux_derive::lookup_lastmod(&flux.blobs, r, p).await?.map(|e| e.cs),
        _ => None,
    };
    let mut want: Vec<ChangesetId> = lm.as_ref().map(|n| n.entries.iter().map(|e| e.cs).collect()).unwrap_or_default();
    want.extend(latest_id);
    want.sort();
    want.dedup();
    let loaded: HashMap<ChangesetId, Arc<Loaded>> =
        flux.load_arc(ctx.id, &want).await?.into_iter().flatten().map(|l| (l.id, l)).collect();
    let mut entries: Vec<&TreeEntry> = t.entries().iter().collect();
    entries.sort_by_key(|e| (!e.is_dir(), e.name.as_bytes().to_ascii_lowercase()));
    let readme = t.entries().iter().filter(|e| !e.is_dir()).find(|e| {
        let n = String::from_utf8_lossy(e.name.as_bytes()).to_ascii_lowercase();
        matches!(n.as_str(), "readme.md" | "readme.markdown" | "readme" | "readme.txt" | "readme.rst")
    });
    let readme_html = match readme.and_then(|e| e.content_id().map(|c| (e, c))) {
        Some((e, cid)) if e.size <= 512 * 1024 => {
            let data = flux.blobs.get_content_bytes(&cid).await?.unwrap_or_default();
            let text = String::from_utf8_lossy(&data);
            let name = String::from_utf8_lossy(e.name.as_bytes()).to_string();
            let dir_s = path.as_ref().map(|p| p.to_string()).unwrap_or_default();
            let blob_base = format!("/{}/-/blob/{}", enc(&ctx.name), enc(rev));
            let raw_base = format!("/{}/-/raw/{}", enc(&ctx.name), enc(rev));
            let html_out = if name.to_ascii_lowercase().ends_with(".md") || name.to_ascii_lowercase().ends_with(".markdown") {
                markdown::render(&text, Some(&LinkBase { blob: &blob_base, raw: &raw_base, dir: &dir_s }))
            } else {
                html! { pre.plain { (text) } }.into_string()
            };
            Some((name, html_out))
        }
        _ => None,
    };
    let history_url = url(&ctx.name, "commits", rev, path.as_ref());
    let total_files = t.total_files();
    let body = html! {
        div.container {
            div.toolbar {
                (ref_picker(ctx, rev, "tree", path.as_ref()))
                (breadcrumbs(&ctx.name, rev, path.as_ref()))
                div.spacer {}
                span.muted.small { (view::count(total_files)) " files · " (view::size(t.total_size())) }
            }
            div.card.files {
                @match latest_id.and_then(|id| loaded.get(&id)) {
                    Some(l) => (commit_bar(ctx, l, rev, &history_url)),
                    None => div.commit-bar.muted { "Indexing history…" },
                }
                table.file-table {
                    tbody {
                        @if let Some(p) = &path {
                            tr {
                                td.name colspan="3" {
                                    a.up href=(url(&ctx.name, "tree", rev, p.parent().as_ref())) { (icon("dir")) " .." }
                                }
                            }
                        }
                        @for e in &entries {
                            @let child = match &path { Some(p) => p.join(e.name.as_bytes()).ok(), None => RepoPath::new(e.name.as_bytes()).ok() };
                            @let action = if e.is_dir() { "tree" } else { "blob" };
                            @let lme = lm.as_ref().and_then(|n| n.get(e.name.as_bytes()));
                            @let cl = lme.and_then(|x| loaded.get(&x.cs));
                            tr {
                                td.name {
                                    a href=(url(&ctx.name, action, rev, child.as_ref())) class=(if e.is_dir() { "dir" } else { "f" }) {
                                        (icon(kind_icon(e)))
                                        span { (String::from_utf8_lossy(e.name.as_bytes())) }
                                    }
                                }
                                td.msg {
                                    @if let Some(l) = cl {
                                        a href=(url(&ctx.name, "commit", &l.id.to_hex(), None)) { (subject(&l.changeset.message)) }
                                    } @else if lm.is_none() {
                                        span.skeleton {}
                                    }
                                }
                                td.when {
                                    @if let Some(l) = cl { (time(l.changeset.author_time.unix_ms)) }
                                }
                            }
                        }
                    }
                }
            }
            @if let Some((name, html_out)) = &readme_html {
                div.card.readme {
                    div.card-head { (icon("file")) " " (name) }
                    article.markdown { (PreEscaped(html_out)) }
                }
            }
        }
    };
    let title = match &path {
        Some(p) => format!("{p} at {} · {}", rev_label(rev), ctx.name),
        None => ctx.name.clone(),
    };
    let n = nav(web, ctx, Tab::Code, rev).await;
    Ok(page(web, req, &title, Some(n), body))
}

fn is_image(path: &RepoPath) -> Option<&'static str> {
    let p = path.to_string().to_ascii_lowercase();
    let ext = p.rsplit_once('.').map(|(_, e)| e)?;
    Some(match ext {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "svg" => "image/svg+xml",
        "avif" => "image/avif",
        "bmp" => "image/bmp",
        _ => return None,
    })
}

async fn file_entry(web: &WebState, ctx: &RepoCtx, rev: &str, path: &RepoPath) -> WebResult<(ChangesetId, TreeEntry)> {
    let cs = web.flux.resolve_rev(ctx.id, rev).await?;
    let e = web
        .flux
        .entry_at(ctx.id, &cs, path)
        .await?
        .ok_or_else(|| WebError::NotFound(format!("{path} does not exist at {}", rev_label(rev))))?;
    Ok((cs, e))
}

fn code_table(lines: &[String], highlighted: bool, plain: &[&str], blame: Option<&[Markup]>) -> Markup {
    html! {
        table.code.blame[blame.is_some()] {
            tbody {
                @for i in 0..plain.len().min(MAX_VIEW_LINES) {
                    tr id=(format!("L{}", i + 1)) {
                        @if let Some(b) = blame { (b[i]) }
                        td.ln data-line=(i + 1) {}
                        td.lc {
                            @if highlighted { (PreEscaped(&lines[i])) } @else { (plain[i]) }
                        }
                    }
                }
            }
        }
    }
}

async fn blob(web: &WebState, req: &Req, ctx: &RepoCtx, rev: &str, path: &RepoPath, plain_md: bool) -> WebResult {
    let flux = &web.flux;
    let (cs, entry) = file_entry(web, ctx, rev, path).await?;
    if entry.is_dir() {
        return Ok(Redirect::to(&url(&ctx.name, "tree", rev, Some(path))).into_response());
    }
    let lm_root = lastmod_ready(flux, ctx.id, &cs).await;
    let last = match lm_root {
        Some(r) => match flux_derive::lookup_lastmod(&flux.blobs, r, path).await? {
            Some(e) => flux.load_arc(ctx.id, &[e.cs]).await?.pop().flatten(),
            None => None,
        },
        None => None,
    };
    let history_url = url(&ctx.name, "commits", rev, Some(path));
    let raw_url = url(&ctx.name, "raw", rev, Some(path));
    let content_block: Markup;
    let mut hl_pending = false;
    let mut info: Vec<String> = Vec::new();
    match (entry.file_kind(), entry.content_id()) {
        (Some(FileKind::GitLink(sha)), _) => {
            content_block = html! { div.notice { (icon("module")) " Submodule at " code { (hex::encode(sha)) } } };
        }
        (_, Some(cid)) => {
            let data = flux.blobs.get_content_bytes(&cid).await?.unwrap_or_default();
            info.push(view::size(entry.size));
            if let Some(mime) = is_image(path) {
                let _ = mime;
                content_block = html! { div.image-view { img src=(raw_url) alt=(path.basename().escape_ascii().to_string()); } };
            } else if is_binary(&data) {
                content_block = html! { div.notice { "Binary file not shown. " a href=(raw_url) { "Download" } } };
            } else {
                let text = String::from_utf8_lossy(&data);
                let is_md = {
                    let p = path.to_string().to_ascii_lowercase();
                    p.ends_with(".md") || p.ends_with(".markdown")
                };
                let plain: Vec<&str> = {
                    let mut v: Vec<&str> = text.split('\n').collect();
                    if v.len() > 1 && v.last() == Some(&"") {
                        v.pop();
                    }
                    v
                };
                info.insert(0, format!("{} lines", plain.len()));
                if entry.file_kind() == Some(FileKind::Symlink) {
                    content_block = html! { div.notice { (icon("link")) " Symbolic link to " code { (text) } } };
                } else if is_md && !plain_md {
                    let dir_s = path.parent().map(|p| p.to_string()).unwrap_or_default();
                    let blob_base = format!("/{}/-/blob/{}", enc(&ctx.name), enc(rev));
                    let raw_base = format!("/{}/-/raw/{}", enc(&ctx.name), enc(rev));
                    let out = markdown::render(&text, Some(&LinkBase { blob: &blob_base, raw: &raw_base, dir: &dir_s }));
                    content_block = html! { article.markdown.padded { (PreEscaped(out)) } };
                } else {
                    let (hl, pending) = highlight_bounded(web, cid, &path.to_string(), &text).await;
                    hl_pending = pending;
                    if let Some(lang) = web.hl.language(&path.to_string(), &text) {
                        info.push(lang);
                    }
                    let truncated = plain.len() > MAX_VIEW_LINES;
                    content_block = html! {
                        @match &hl {
                            Some(lines) => (code_table(lines, true, &plain, None)),
                            None => (code_table(&[], false, &plain, None)),
                        }
                        @if truncated { div.notice { "Showing the first " (MAX_VIEW_LINES) " lines. " a href=(raw_url) { "View raw" } } }
                    };
                }
            }
        }
        _ => content_block = html! { div.notice { "Empty" } },
    }
    let is_md = path.to_string().to_ascii_lowercase().ends_with(".md");
    let permalink = url(&ctx.name, "blob", &cs.to_hex(), Some(path));
    let body = html! {
        div.container {
            div.toolbar {
                (ref_picker(ctx, rev, "blob", Some(path)))
                (breadcrumbs(&ctx.name, rev, Some(path)))
            }
            div.card {
                @if let Some(l) = &last { (commit_bar(ctx, l, rev, &history_url)) }
                div.file-head {
                    span.muted { (info.join(" · ")) }
                    div.spacer {}
                    @if is_md {
                        div.seg {
                            a.active[!plain_md] href=(url(&ctx.name, "blob", rev, Some(path))) { "Preview" }
                            a.active[plain_md] href=(format!("{}?plain=1", url(&ctx.name, "blob", rev, Some(path)))) { "Code" }
                        }
                    }
                    a.btn.small href=(raw_url) { (icon("raw")) "Raw" }
                    a.btn.small href=(url(&ctx.name, "blame", rev, Some(path))) { (icon("blame")) "Blame" }
                    a.btn.small href=(history_url) { (icon("history")) "History" }
                    button.btn.small.copy-btn type="button" data-copy-url=(permalink) title="Copy permalink (y)" { (icon("copy")) "Permalink" }
                }
                (content_block)
            }
            @if hl_pending { div.hl-pending hidden {} }
        }
    };
    let n = nav(web, ctx, Tab::Code, rev).await;
    Ok(page(web, req, &format!("{path} at {} · {}", rev_label(rev), ctx.name), Some(n), body))
}

/// Highlight within a small time budget. Large files keep highlighting in
/// the background (the result is cached by content id); the page renders
/// plain text now and the client swaps in the highlighted version.
async fn highlight_bounded(web: &WebState, id: flux_core::ContentId, path: &str, text: &str) -> (Option<Arc<Vec<String>>>, bool) {
    if let Some(v) = web.hl.cached(id, path, text) {
        return (Some(v), false);
    }
    if !web.hl.has_grammar(path, text) {
        return (None, false);
    }
    let (w, p, t) = (web.clone(), path.to_string(), text.to_string());
    let task = tokio::task::spawn_blocking(move || w.hl.lines(id, &p, &t));
    match tokio::time::timeout(std::time::Duration::from_millis(50), task).await {
        Ok(Ok(v)) => (v, false),
        _ => (None, true),
    }
}

async fn raw(web: &WebState, ctx: &RepoCtx, rev: &str, path: &RepoPath) -> WebResult {
    let (cs, entry) = file_entry(web, ctx, rev, path).await?;
    let cid = entry.content_id().ok_or_else(|| WebError::BadRequest(format!("{path} has no content")))?;
    let data = web.flux.blobs.get_content_bytes(&cid).await?.unwrap_or_default();
    let ctype = match is_image(path) {
        Some(m) => m,
        None if is_binary(&data) => "application/octet-stream",
        None => "text/plain; charset=utf-8",
    };
    let mut r = data.into_response();
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(ctype));
    // Repository content must never run as this origin (SVG/HTML XSS).
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'none'; img-src 'self'; style-src 'unsafe-inline'; sandbox"),
    );
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::ETAG, HeaderValue::from_str(&format!("\"{}\"", cid.to_hex())).unwrap());
    let immutable = rev == cs.to_hex();
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(if immutable { "public, max-age=31536000, immutable" } else { "public, max-age=30" }),
    );
    Ok(r)
}

async fn blame(web: &WebState, req: &Req, ctx: &RepoCtx, rev: &str, path: &RepoPath) -> WebResult {
    let flux = &web.flux;
    let (cs, entry) = file_entry(web, ctx, rev, path).await?;
    let cid = entry.content_id().ok_or_else(|| WebError::BadRequest(format!("{path} is not a file")))?;
    let data = flux.blobs.get_content_bytes(&cid).await?.unwrap_or_default();
    if is_binary(&data) {
        return Err(WebError::BadRequest("cannot blame a binary file".into()));
    }
    let b = flux.blame(ctx.id, &cs, path).await?;
    let mut ids: Vec<ChangesetId> = b.ranges.iter().map(|r| r.2).collect();
    ids.sort();
    ids.dedup();
    let loaded: HashMap<ChangesetId, Arc<Loaded>> =
        flux.load_arc(ctx.id, &ids).await?.into_iter().flatten().map(|l| (l.id, l)).collect();
    let (min_t, max_t) = loaded.values().fold((i64::MAX, i64::MIN), |(lo, hi), l| {
        let t = l.changeset.author_time.unix_ms;
        (lo.min(t), hi.max(t))
    });
    let text = String::from_utf8_lossy(&data);
    let mut plain: Vec<&str> = text.split('\n').collect();
    if plain.len() > 1 && plain.last() == Some(&"") {
        plain.pop();
    }
    let (hl, hl_pending) = highlight_bounded(web, cid, &path.to_string(), &text).await;
    let mut gutter: Vec<Markup> = vec![html! { td.bl {} }; plain.len()];
    for (start, count, id) in &b.ranges {
        let l = loaded.get(id);
        let age = l
            .map(|l| if max_t > min_t { ((l.changeset.author_time.unix_ms - min_t) * 9 / (max_t - min_t)) as u32 } else { 9 })
            .unwrap_or(0);
        for k in 0..*count {
            let i = (*start + k) as usize;
            if i >= gutter.len() {
                break;
            }
            gutter[i] = if k == 0 {
                html! {
                    td class=(format!("bl first age{age}")) {
                        @if let Some(l) = l {
                            @let (n, e) = person(&l.changeset.author);
                            div.bl-row {
                                (avatar(&n, &e, false))
                                a.bl-msg href=(url(&ctx.name, "commit", &l.id.to_hex(), None)) title=(subject(&l.changeset.message)) { (subject(&l.changeset.message)) }
                                span.bl-when { (time(l.changeset.author_time.unix_ms)) }
                            }
                        }
                    }
                }
            } else {
                html! { td class=(format!("bl age{age}")) {} }
            };
        }
    }
    let body = html! {
        div.container.wide {
            div.toolbar {
                (ref_picker(ctx, rev, "blame", Some(path)))
                (breadcrumbs(&ctx.name, rev, Some(path)))
                div.spacer {}
                a.btn.small href=(url(&ctx.name, "blob", rev, Some(path))) { (icon("code")) "Code" }
            }
            div.card {
                div.file-head { span.muted { (plain.len()) " lines · " (ids.len()) " changesets" } div.spacer {} span.legend { "older" span.age-scale {} "newer" } }
                @match &hl {
                    Some(lines) => (code_table(lines, true, &plain, Some(&gutter))),
                    None => (code_table(&[], false, &plain, Some(&gutter))),
                }
            }
            @if hl_pending { div.hl-pending hidden {} }
        }
    };
    let n = nav(web, ctx, Tab::Code, rev).await;
    Ok(page(web, req, &format!("Blame {path} · {}", ctx.name), Some(n), body))
}

// ───────────── history ─────────────

fn commit_row(ctx: &RepoCtx, l: &Loaded, rev_for_tree: bool) -> Markup {
    let (n, e) = person(&l.changeset.author);
    let id = l.id.to_hex();
    html! {
        li.commit {
            (avatar(&n, &e, false))
            div.commit-main {
                a.commit-title href=(url(&ctx.name, "commit", &id, None)) { (subject(&l.changeset.message)) }
                div.commit-meta {
                    strong { (n) } " committed " (time(l.changeset.author_time.unix_ms))
                    @if l.changeset.parents.len() > 1 { span.pill.small { (icon("merge")) "merge" } }
                }
            }
            div.commit-actions {
                button.sha.copy-btn type="button" data-copy=(id) title="Copy changeset id" { (short(&l.id)) }
                @if rev_for_tree {
                    a.btn.small.icon-only href=(url(&ctx.name, "tree", &id, None)) title="Browse files at this changeset" { (icon("code")) }
                }
            }
        }
    }
}

fn commit_list(ctx: &RepoCtx, items: &[Arc<Loaded>]) -> Markup {
    let mut groups: Vec<(i64, Vec<&Arc<Loaded>>)> = Vec::new();
    for l in items {
        let d = view::day_key(l.changeset.author_time.unix_ms);
        match groups.last_mut() {
            Some((k, v)) if *k == d => v.push(l),
            _ => groups.push((d, vec![l])),
        }
    }
    html! {
        @for (_, group) in &groups {
            div.day {
                div.day-label { (icon("commit")) "Changes on " (view::day_label(group[0].changeset.author_time.unix_ms)) }
                ul.commits.card { @for l in group { (commit_row(ctx, l, true)) } }
            }
        }
    }
}

async fn commits(web: &WebState, req: &Req, ctx: &RepoCtx, rev: &str, path: Option<RepoPath>, page_no: usize) -> WebResult {
    let flux = &web.flux;
    let cs = flux.resolve_rev(ctx.id, rev).await?;
    if path.is_some() {
        // Path history needs lastmod; derive now (fast after the first time).
        flux.lastmod(ctx.id, &cs).await?;
    }
    let items = flux.log_page(ctx.id, &cs, &[], path.as_ref(), false, page_no * PAGE_SIZE, PAGE_SIZE + 1).await?;
    let more = items.len() > PAGE_SIZE;
    let items = &items[..items.len().min(PAGE_SIZE)];
    let base = url(&ctx.name, "commits", rev, path.as_ref());
    let body = html! {
        div.container {
            div.toolbar {
                (ref_picker(ctx, rev, "commits", path.as_ref()))
                @if let Some(p) = &path {
                    div.crumbs-label { "History for " (breadcrumbs(&ctx.name, rev, Some(p))) }
                }
            }
            @if items.is_empty() { (empty_state("No changes", html! { p { "Nothing has touched this path yet." } })) }
            (commit_list(ctx, items))
            div.pager {
                @if page_no > 0 { a.btn href=(format!("{base}?page={}", page_no - 1)) { "← Newer" } }
                @if more { a.btn href=(format!("{base}?page={}", page_no + 1)) { "Older →" } }
            }
        }
    };
    let n = nav(web, ctx, Tab::Commits, rev).await;
    Ok(page(web, req, &format!("Changes · {}", ctx.name), Some(n), body))
}

struct RenderedFile {
    path: RepoPath,
    status: &'static str,
    old: Option<flux_derive::TreeFile>,
    new: Option<flux_derive::TreeFile>,
    diff: Option<FileDiff>,
    note: Option<String>,
}

/// Diff two trees into renderable file diffs, fetching contents in
/// parallel and stopping detailed diffs once the line budget is spent.
async fn diff_files(web: &WebState, old: Option<TreeId>, new: Option<TreeId>) -> WebResult<Vec<RenderedFile>> {
    let all = |_: &RepoPath, _: bool| true;
    let entries = diff_trees(&web.flux.blobs, old, new, &all).await?;
    type Fetched = Option<(flux_core::ContentId, bytes::Bytes)>;
    async fn fetch(blobs: flux_store::BlobStore, id: Option<flux_core::ContentId>) -> Result<Fetched, flux_store::StoreError> {
        match id {
            Some(id) => blobs.get_content_bytes(&id).await.map(|b| b.map(|b| (id, b))),
            None => Ok(None),
        }
    }
    let want = |f: &Option<flux_derive::TreeFile>| f.as_ref().filter(|f| f.size <= MAX_DIFF_FILE).and_then(|f| f.content_id);
    let futs: Vec<_> = entries
        .iter()
        .take(1000)
        .map(|e| {
            let (a, b) = (fetch(web.flux.blobs.clone(), want(&e.old)), fetch(web.flux.blobs.clone(), want(&e.new)));
            Box::pin(async move { Ok::<(Fetched, Fetched), flux_store::StoreError>((a.await?, b.await?)) })
        })
        .collect();
    use futures::StreamExt;
    let fetched: Vec<Result<(Fetched, Fetched), flux_store::StoreError>> =
        futures::stream::iter(futs).buffered(32).collect().await;
    let mut budget = DIFF_LINE_BUDGET;
    let mut out = Vec::with_capacity(entries.len());
    for (i, e) in entries.into_iter().enumerate() {
        let status = match (&e.old, &e.new) {
            (None, Some(_)) => "added",
            (Some(_), None) => "deleted",
            _ => "modified",
        };
        let mut rf =
            RenderedFile { path: e.path.clone(), status, old: e.old.clone(), new: e.new.clone(), diff: None, note: None };
        let Some(Ok((a, b))) = fetched.get(i) else {
            rf.note = Some("Too many files; diff not shown.".into());
            out.push(rf);
            continue;
        };
        let too_big =
            e.old.as_ref().is_some_and(|f| f.size > MAX_DIFF_FILE) || e.new.as_ref().is_some_and(|f| f.size > MAX_DIFF_FILE);
        let a_bytes = a.as_ref().map(|x| x.1.clone()).unwrap_or_default();
        let b_bytes = b.as_ref().map(|x| x.1.clone()).unwrap_or_default();
        if too_big {
            rf.note = Some("Large file; diff not shown.".into());
        } else if is_binary(&a_bytes) || is_binary(&b_bytes) {
            rf.note = Some("Binary file".into());
        } else if budget == 0 {
            rf.note = Some("Diff not shown to keep this page fast.".into());
            // Still count lines.
            let d = diff_text(&String::from_utf8_lossy(&a_bytes), &String::from_utf8_lossy(&b_bytes), 0);
            rf.diff = Some(FileDiff { hunks: vec![], ..d });
        } else {
            let d = diff_text(&String::from_utf8_lossy(&a_bytes), &String::from_utf8_lossy(&b_bytes), 3);
            let n: usize = d.hunks.iter().map(|h| h.lines.len()).sum();
            budget = budget.saturating_sub(n);
            rf.diff = Some(d);
        }
        out.push(rf);
    }
    Ok(out)
}

fn diff_view(ctx: &RepoCtx, rev: &str, files: &[RenderedFile]) -> Markup {
    let added: u32 = files.iter().filter_map(|f| f.diff.as_ref()).map(|d| d.added).sum();
    let deleted: u32 = files.iter().filter_map(|f| f.diff.as_ref()).map(|d| d.deleted).sum();
    html! {
        div.diff-summary {
            strong { (files.len()) " file" @if files.len() != 1 { "s" } " changed" }
            span.add { "+" (added) } span.del { "−" (deleted) }
            div.spacer {}
            div.seg.diff-mode { button.active type="button" data-mode="unified" { "Unified" } button type="button" data-mode="split" { "Split" } }
        }
        details.file-index open[files.len() <= 30] {
            summary { "Files" }
            ul {
                @for (i, f) in files.iter().enumerate() {
                    li {
                        span class=(format!("st {}", f.status)) {}
                        a href=(format!("#diff-{i}")) { (f.path.to_string()) }
                        @if let Some(d) = &f.diff { span.add { "+" (d.added) } span.del { "−" (d.deleted) } }
                    }
                }
            }
        }
        @for (i, f) in files.iter().enumerate() {
            div.card.diff-file id=(format!("diff-{i}")) {
                div.diff-head {
                    button.fold type="button" aria-label="Collapse" {}
                    span class=(format!("st {}", f.status)) {}
                    strong.path { (f.path.to_string()) }
                    @if let (Some(o), Some(n)) = (&f.old, &f.new) {
                        @if o.kind != n.kind { span.pill.small { "mode changed" } }
                    }
                    @if let Some(d) = &f.diff { span.add { "+" (d.added) } span.del { "−" (d.deleted) } }
                    div.spacer {}
                    @if f.new.is_some() {
                        a.btn.small href=(url(&ctx.name, "blob", rev, Some(&f.path))) { "View file" }
                    }
                }
                @if let Some(note) = &f.note {
                    div.notice.small { (note) }
                }
                @if let Some(d) = &f.diff {
                    @if !d.hunks.is_empty() {
                        table.diff {
                            @for h in &d.hunks {
                                tbody {
                                    tr.hunk { td.ln {} td.ln {} td.lc { "@@ -" (h.old_start) " +" (h.new_start) " @@" } }
                                    @for l in &h.lines {
                                        tr class=(match l.kind { LineKind::Add => "l-add", LineKind::Del => "l-del", LineKind::Context => "l-ctx" }) {
                                            td.ln data-line=[l.old] {}
                                            td.ln data-line=[l.new] {}
                                            td.lc { span.mark { (match l.kind { LineKind::Add => "+", LineKind::Del => "-", LineKind::Context => " " }) } (l.text) }
                                        }
                                    }
                                }
                            }
                        }
                    } @else if f.note.is_none() && f.status == "modified" {
                        div.notice.small { "File mode changed; contents are identical." }
                    }
                }
            }
        }
    }
}

async fn commit(web: &WebState, req: &Req, ctx: &RepoCtx, rev: &str) -> WebResult {
    let flux = &web.flux;
    let cs = flux.resolve_rev(ctx.id, rev).await?;
    let l = flux.load_one_arc(ctx.id, &cs).await?;
    let tree = flux.tree_of(ctx.id, &cs).await?;
    let parent_tree = match l.changeset.parents.first() {
        Some(p) => Some(flux.tree_of(ctx.id, p).await?),
        None => None,
    };
    let files = diff_files(web, parent_tree, Some(tree)).await?;
    let git = flux.meta.cs_to_git(ctx.id, &[cs]).await?.pop().flatten();
    let (n, e) = person(&l.changeset.author);
    let committer = l.changeset.committer.as_ref().map(|c| person(c).0).filter(|c| *c != n);
    let rest = view::body_text(&l.changeset.message);
    let hex = cs.to_hex();
    let rebased_from = l.changeset.extra.get("flux.rebased_from").map(|v| String::from_utf8_lossy(v).into_owned());
    let body = html! {
        div.container {
            div.card.commit-card {
                div.commit-card-head {
                    h1 { (subject(&l.changeset.message)) }
                    a.btn.small href=(url(&ctx.name, "tree", &hex, None)) { (icon("code")) "Browse files" }
                }
                @if !rest.is_empty() { pre.commit-body { (rest) } }
                div.commit-card-meta {
                    (avatar(&n, &e, false))
                    strong { (n) }
                    span.muted { " authored " (time(l.changeset.author_time.unix_ms)) }
                    @if let Some(c) = committer { span.muted { " · committed by " strong { (c) } } }
                    div.spacer {}
                    div.ids {
                        @for (i, p) in l.changeset.parents.iter().enumerate() {
                            span.muted { @if i == 0 { "parent " } @else { " + " } }
                            a.sha href=(url(&ctx.name, "commit", &p.to_hex(), None)) { (short(p)) }
                        }
                        span.muted { " changeset " }
                        button.sha.copy-btn type="button" data-copy=(hex) { (short(&cs)) }
                        @if let Some(g) = git { span.muted { " git " } button.sha.copy-btn type="button" data-copy=(hex::encode(g)) { (&hex::encode(g)[..10]) } }
                    }
                }
                @if let Some(from) = rebased_from {
                    div.notice.small { "Rebased by the server from " code { (&from[..from.len().min(10)]) } }
                }
            }
            (diff_view(ctx, &hex, &files))
        }
    };
    let n = nav(web, ctx, Tab::Commits, rev).await;
    Ok(page(web, req, &format!("{} · {}", subject(&l.changeset.message), ctx.name), Some(n), body))
}

async fn compare(web: &WebState, req: &Req, ctx: &RepoCtx, spec: &str) -> WebResult {
    let flux = &web.flux;
    let default = ctx.default_branch().unwrap_or("main").to_string();
    let (base, head) = match spec.split_once("...") {
        Some((a, b)) => (a.to_string(), b.to_string()),
        None if !spec.is_empty() => (default.clone(), spec.to_string()),
        None => (default.clone(), default.clone()),
    };
    let b = flux.resolve_rev(ctx.id, &base).await?;
    let h = flux.resolve_rev(ctx.id, &head).await?;
    let mb = flux.merge_base(ctx.id, &b, &h).await?;
    let items = flux.log_page(ctx.id, &h, &[b], None, false, 0, 250).await?;
    let files = match mb {
        Some(m) => diff_files(web, Some(flux.tree_of(ctx.id, &m).await?), Some(flux.tree_of(ctx.id, &h).await?)).await?,
        None => vec![],
    };
    let can_write = req.principal.can_write();
    let body = html! {
        div.container {
            div.page-head {
                h1 { "Compare changes" }
                form.compare-form method="get" action=(format!("/{}/-/compare", enc(&ctx.name))) {
                    span.pill { "base: " (base) } span.muted { " ← " } span.pill { "head: " (head) }
                }
            }
            @if items.is_empty() {
                (empty_state("Nothing to compare", html! { p { strong { (head) } " has no changes that " strong { (base) } " doesn't already have." } }))
            } @else {
                @if can_write {
                    div.card.cta {
                        div { strong { (items.len()) " changeset" @if items.len() != 1 { "s" } } " ready to review" }
                        a.btn.primary href=(format!("/{}/-/reviews/new?head={}&target={}", enc(&ctx.name), enc(&head), enc(&base))) { (icon("review")) "Open a review" }
                    }
                }
                (commit_list(ctx, &items))
                (diff_view(ctx, &h.to_hex(), &files))
            }
        }
    };
    let n = nav(web, ctx, Tab::Commits, &head).await;
    Ok(page(web, req, &format!("Compare {base}...{head} · {}", ctx.name), Some(n), body))
}

async fn branches(web: &WebState, req: &Req, ctx: &RepoCtx) -> WebResult {
    let flux = &web.flux;
    let default = ctx.default_branch().map(str::to_string);
    let base = match &default {
        Some(d) => ctx.refs.iter().find(|(n, _)| n == d).map(|r| r.1),
        None => None,
    };
    let ids: Vec<ChangesetId> = ctx.refs.iter().map(|r| r.1).collect();
    let loaded: HashMap<ChangesetId, Arc<Loaded>> =
        flux.load_arc(ctx.id, &ids).await?.into_iter().flatten().map(|l| (l.id, l)).collect();
    let mut rows: Vec<(&String, &ChangesetId, Option<&Arc<Loaded>>, Option<(usize, usize)>)> = Vec::new();
    for (i, (name, id)) in ctx.refs.iter().enumerate() {
        let ab = match base {
            Some(b) if i < 200 => flux.ahead_behind(ctx.id, id, &b).await.ok(),
            _ => None,
        };
        rows.push((name, id, loaded.get(id), ab));
    }
    rows.sort_by_key(|r| std::cmp::Reverse(r.2.map(|l| l.changeset.author_time.unix_ms).unwrap_or(0)));
    let body = html! {
        div.container {
            div.page-head { h1 { "Branches" } p.muted { (ctx.refs.len()) " refs" } }
            div.card {
                table.branches {
                    tbody {
                        @for (name, id, l, ab) in &rows {
                            tr {
                                td.name {
                                    a.branch href=(url(&ctx.name, "tree", name, None)) { (icon("branch")) (name) }
                                    @if Some(name.as_str()) == default.as_deref() { span.pill.small { "default" } }
                                }
                                td.msg {
                                    @if let Some(l) = l {
                                        a href=(url(&ctx.name, "commit", &id.to_hex(), None)) { (subject(&l.changeset.message)) }
                                    }
                                }
                                td.when { @if let Some(l) = l { (time(l.changeset.author_time.unix_ms)) } }
                                td.ab {
                                    @if let Some((a, b)) = ab {
                                        @if Some(name.as_str()) != default.as_deref() {
                                            span.behind title="Behind the default branch" { (b) }
                                            span.bar {}
                                            span.ahead title="Ahead of the default branch" { (a) }
                                        }
                                    }
                                }
                                td.act {
                                    @if Some(name.as_str()) != default.as_deref() {
                                        @if let Some(d) = &default {
                                            a.btn.small href=(format!("/{}/-/compare/{}...{}", enc(&ctx.name), enc(d), enc(name))) { "Compare" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    };
    let rev = default.unwrap_or_else(|| "main".into());
    let n = nav(web, ctx, Tab::Branches, &rev).await;
    Ok(page(web, req, &format!("Branches · {}", ctx.name), Some(n), body))
}

async fn activity(web: &WebState, req: &Req, ctx: &RepoCtx, before: Option<i64>) -> WebResult {
    let flux = &web.flux;
    let log = flux.meta.ref_log(ctx.id, before, 100).await?;
    let mut ids: Vec<ChangesetId> = log.iter().filter_map(|r| r.new).collect();
    ids.sort();
    ids.dedup();
    let loaded: HashMap<ChangesetId, Arc<Loaded>> =
        flux.load_arc(ctx.id, &ids).await?.into_iter().flatten().map(|l| (l.id, l)).collect();
    let body = html! {
        div.container {
            div.page-head { h1 { "Activity" } p.muted { "Every ref move, newest first. Updates live." } }
            ul.card.activity {
                @for r in &log {
                    @let name = String::from_utf8_lossy(&r.name).into_owned();
                    li {
                        (avatar(&r.actor, "", false))
                        div.act-main {
                            div {
                                strong { (r.actor) }
                                @match (r.old, r.new) {
                                    (None, Some(_)) => { " created " }
                                    (Some(_), None) => { " deleted " }
                                    _ => { " moved " }
                                }
                                a.branch href=(url(&ctx.name, "tree", &name, None)) { (icon("branch")) (name) }
                                @if let (Some(o), Some(n)) = (r.old, r.new) {
                                    " " a.sha href=(format!("/{}/-/compare/{}...{}", enc(&ctx.name), o.to_hex(), n.to_hex())) { (short(&o)) "…" (short(&n)) }
                                }
                            }
                            div.muted.small {
                                @if let Some(l) = r.new.and_then(|n| loaded.get(&n)) { (subject(&l.changeset.message)) " · " }
                                (r.reason) " · " (time(r.ts_ms))
                            }
                        }
                    }
                }
            }
            @if log.len() == 100 {
                div.pager { a.btn href=(format!("/{}/-/activity?before={}", enc(&ctx.name), log.last().unwrap().seq)) { "Older →" } }
            }
        }
    };
    let rev = ctx.default_branch().unwrap_or("main").to_string();
    let n = nav(web, ctx, Tab::Activity, &rev).await;
    Ok(page(web, req, &format!("Activity · {}", ctx.name), Some(n), body))
}

async fn search(web: &WebState, req: &Req, ctx: &RepoCtx, q: &HashMap<String, String>) -> WebResult {
    let flux = &web.flux;
    let query = q.get("q").cloned().unwrap_or_default();
    let rev =
        q.get("rev").filter(|r| !r.is_empty()).cloned().unwrap_or_else(|| ctx.default_branch().unwrap_or("main").to_string());
    let regex = q.get("regex").is_some_and(|v| v == "1" || v == "on");
    let case = q.get("case").is_some_and(|v| v == "1" || v == "on");
    let prefix = q
        .get("path")
        .filter(|p| !p.trim_matches('/').is_empty())
        .map(|p| RepoPath::new(p.trim_matches('/').as_bytes()))
        .transpose()
        .map_err(|e| WebError::BadRequest(e.to_string()))?;
    let started = std::time::Instant::now();
    let result = if query.is_empty() {
        None
    } else {
        let cs = flux.resolve_rev(ctx.id, &rev).await?;
        let root = flux.tree_of(ctx.id, &cs).await?;
        Some(flux.search(ctx.id, root, &query, regex, !case, prefix.as_ref(), 1000).await?)
    };
    let elapsed = started.elapsed();
    let mut by_file: BTreeMap<String, Vec<&crate::search::SearchHit>> = BTreeMap::new();
    if let Some(r) = &result {
        for h in &r.hits {
            by_file.entry(h.path.to_string()).or_default().push(h);
        }
    }
    let body = html! {
        div.container {
            form.search-form method="get" action=(format!("/{}/-/search", enc(&ctx.name))) {
                div.search-row {
                    (icon("search"))
                    input type="search" name="q" value=(query) placeholder="Search code" autofocus;
                    button.btn.primary type="submit" { "Search" }
                }
                div.search-opts {
                    label { input type="checkbox" name="regex" value="1" checked[regex]; " Regex" }
                    label { input type="checkbox" name="case" value="1" checked[case]; " Match case" }
                    label { "in " input type="text" name="path" value=(prefix.as_ref().map(|p| p.to_string()).unwrap_or_default()) placeholder="path/prefix"; }
                    label { "at " input type="text" name="rev" value=(rev); }
                }
            }
            @if let Some(r) = &result {
                p.muted.small {
                    (r.hits.len()) @if r.truncated { "+" } " results in " (by_file.len()) " files · scanned " (r.files_scanned) " files in "
                    (format!("{:.1}", elapsed.as_secs_f64() * 1000.0)) " ms"
                }
                @for (path, hits) in &by_file {
                    div.card.search-file {
                        div.card-head { (icon("file")) a href=(format!("/{}/-/blob/{}/{}", enc(&ctx.name), enc(&rev), enc_path(path.as_bytes()))) { (path) } span.muted.small { " " (hits.len()) } }
                        table.code {
                            tbody {
                                @for h in hits {
                                    tr {
                                        td.ln { a href=(format!("/{}/-/blob/{}/{}#L{}", enc(&ctx.name), enc(&rev), enc_path(path.as_bytes()), h.line_number)) { (h.line_number) } }
                                        td.lc { (PreEscaped(mark_spans(&h.line, &h.spans))) }
                                    }
                                }
                            }
                        }
                    }
                }
                @if r.hits.is_empty() { (empty_state("No results", html! { p { "Nothing matched " code { (query) } " at " (rev_label(&rev)) "." } })) }
            }
        }
    };
    let n = nav(web, ctx, Tab::Search, &rev).await;
    Ok(page(web, req, &format!("Search · {}", ctx.name), Some(n), body))
}

fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            c => out.push(c),
        }
    }
    out
}

fn mark_spans(line: &[u8], spans: &[(u32, u32)]) -> String {
    let mut out = String::new();
    let mut pos = 0usize;
    for (a, b) in spans {
        let (a, b) = ((*a as usize).min(line.len()), (*b as usize).min(line.len()));
        if a < pos {
            continue;
        }
        out.push_str(&escape(&String::from_utf8_lossy(&line[pos..a])));
        out.push_str("<mark>");
        out.push_str(&escape(&String::from_utf8_lossy(&line[a..b])));
        out.push_str("</mark>");
        pos = b;
    }
    out.push_str(&escape(&String::from_utf8_lossy(&line[pos..])));
    out
}

async fn files_json(web: &WebState, ctx: &RepoCtx, rev: &str) -> WebResult {
    let cs = web.flux.resolve_rev(ctx.id, rev).await?;
    let root = web.flux.tree_of(ctx.id, &cs).await?;
    let list = web.flux.file_list(root).await?;
    let paths: Vec<String> = list.iter().map(|(p, _, _)| p.to_string()).collect();
    let body = serde_json::json!({ "rev": rev, "changeset": cs.to_hex(), "paths": paths }).to_string();
    let mut r = body.into_response();
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    let immutable = rev == cs.to_hex();
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(if immutable { "private, max-age=31536000, immutable" } else { "private, max-age=5" }),
    );
    Ok(r)
}

fn events(web: &WebState, ctx: &RepoCtx) -> Response {
    let mut rx = web.flux.subscribe();
    let repo = ctx.name.clone();
    let stream = async_stream(move |tx| async move {
        loop {
            match rx.recv().await {
                Ok(e) if e.repo() == repo => {
                    let pb = crate::grpc::event_pb(&e);
                    let data = serde_json::json!({
                        "kind": pb.kind,
                        "ref": String::from_utf8_lossy(&pb.ref_name),
                        "new": hex::encode(&pb.new),
                        "actor": pb.actor,
                        "count": pb.count,
                        "review": pb.review,
                        "detail": pb.detail,
                    });
                    if tx.send(Ok(SseEvent::default().event("flux").data(data.to_string()))).await.is_err() {
                        return;
                    }
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(_) => return,
            }
        }
    });
    Sse::new(stream).keep_alive(KeepAlive::default()).into_response()
}

fn async_stream<F, Fut>(f: F) -> tokio_stream::wrappers::ReceiverStream<Result<SseEvent, std::convert::Infallible>>
where
    F: FnOnce(tokio::sync::mpsc::Sender<Result<SseEvent, std::convert::Infallible>>) -> Fut,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let (tx, rx) = tokio::sync::mpsc::channel(32);
    tokio::spawn(f(tx));
    tokio_stream::wrappers::ReceiverStream::new(rx)
}

// ───────────── reviews ─────────────

fn state_badge(state: &str) -> Markup {
    let (cls, ic) = match state {
        "open" => ("open", "review"),
        "landed" => ("landed", "merge"),
        _ => ("closed", "x"),
    };
    html! { span class=(format!("state {cls}")) { (icon(ic)) (state) } }
}

async fn reviews(web: &WebState, req: &Req, ctx: &RepoCtx, state: &str) -> WebResult {
    let flux = &web.flux;
    let all = flux.meta.list_reviews(ctx.id, None, 1000).await?;
    let count = |s: &str| all.iter().filter(|r| r.state == s).count();
    let shown: Vec<&flux_store::Review> = all.iter().filter(|r| state == "all" || r.state == state).collect();
    let body = html! {
        div.container {
            div.page-head.row {
                h1 { "Reviews" }
                div.spacer {}
                @if req.principal.can_write() {
                    a.btn.primary href=(format!("/{}/-/reviews/new", enc(&ctx.name))) { (icon("review")) "New review" }
                }
            }
            div.card {
                div.filter-tabs {
                    @for s in ["open", "landed", "closed"] {
                        a.active[s == state] href=(format!("/{}/-/reviews?state={s}", enc(&ctx.name))) { (state_badge(s)) span.count { (count(s)) } }
                    }
                }
                @if shown.is_empty() {
                    div.empty.small { p.muted { "No " (state) " reviews." } }
                }
                ul.reviews {
                    @for r in &shown {
                        li {
                            (state_badge(&r.state))
                            div.review-main {
                                a.review-title href=(format!("/{}/-/reviews/{}", enc(&ctx.name), r.number)) { (r.title) }
                                div.muted.small { "#" (r.number) " opened " (time(r.created_ms)) " by " strong { (r.author) } " · into " code { (String::from_utf8_lossy(&r.target)) } }
                            }
                            span.muted.small { "updated " (time(r.updated_ms)) }
                        }
                    }
                }
            }
        }
    };
    let rev = ctx.default_branch().unwrap_or("main").to_string();
    let n = nav(web, ctx, Tab::Reviews, &rev).await;
    Ok(page(web, req, &format!("Reviews · {}", ctx.name), Some(n), body))
}

async fn new_review(web: &WebState, req: &Req, ctx: &RepoCtx, q: &HashMap<String, String>) -> WebResult {
    req.principal.require_write()?;
    let head = q.get("head").cloned().unwrap_or_default();
    let target = q.get("target").cloned().unwrap_or_else(|| ctx.default_branch().unwrap_or("main").to_string());
    // Prefill the title from the head's message.
    let title = match web.flux.resolve_rev(ctx.id, &head).await {
        Ok(id) => web.flux.load_one_arc(ctx.id, &id).await.map(|l| subject(&l.changeset.message)).unwrap_or_default(),
        Err(_) => String::new(),
    };
    let body = html! {
        div.container.narrow {
            div.page-head { h1 { "New review" } p.muted { "Propose a stack of changesets for landing. flux rebases it onto the target server-side when you land." } }
            form.card.form method="post" action=(format!("/{}/-/reviews", enc(&ctx.name))) {
                label { "Title" input type="text" name="title" value=(title) required autofocus; }
                label { "Description " span.muted { "(Markdown)" } textarea name="body" rows="8" placeholder="What does this change and why?" {} }
                div.row2 {
                    label { "Head (branch or changeset)" input type="text" name="head" value=(head) required placeholder="feature/x or cs_…"; }
                    label { "Land into"
                        select name="target" {
                            @for (n, _) in ctx.refs.iter() { option value=(n) selected[*n == target] { (n) } }
                            @if !ctx.is_ref(&target) { option value=(target) selected { (target) } }
                        }
                    }
                }
                button.btn.primary type="submit" { "Create review" }
            }
        }
    };
    let n = nav(web, ctx, Tab::Reviews, &target).await;
    Ok(page(web, req, &format!("New review · {}", ctx.name), Some(n), body))
}

async fn review(web: &WebState, req: &Req, ctx: &RepoCtx, number: i64, files_tab: bool) -> WebResult {
    let flux = &web.flux;
    let d = flux.review_detail(ctx.id, number).await?;
    let r = &d.review;
    let target = String::from_utf8_lossy(&r.target).into_owned();
    let can_write = req.principal.can_write();
    // Land readiness.
    let status: Option<(bool, usize, usize)> = match (r.state.as_str(), d.target_head) {
        ("open", Some(h)) => {
            let ff = flux.is_ancestor(ctx.id, &h, &r.head).await.unwrap_or(false);
            let (a, b) = flux.ahead_behind(ctx.id, &r.head, &h).await.unwrap_or((0, 0));
            Some((ff, a, b))
        }
        _ => None,
    };
    let files = if files_tab {
        let (top, base) = match (r.state.as_str(), r.landed) {
            ("landed", Some(l)) => (l, d.stack.last().and_then(|c| c.changeset.parents.first().copied())),
            _ => (
                r.head,
                match d.target_head {
                    Some(h) => flux.merge_base(ctx.id, &h, &r.head).await?,
                    None => None,
                },
            ),
        };
        let old = match base {
            Some(b) => Some(flux.tree_of(ctx.id, &b).await?),
            None => None,
        };
        Some(diff_files(web, old, Some(flux.tree_of(ctx.id, &top).await?)).await?)
    } else {
        None
    };
    let base_url = format!("/{}/-/reviews/{}", enc(&ctx.name), r.number);
    let approvals: Vec<&str> = d.events.iter().filter(|e| e.kind == "approved").map(|e| e.author.as_str()).collect();
    let body = html! {
        div.container {
            div.review-head {
                h1 { (r.title) span.muted { " #" (r.number) } }
                div.review-sub {
                    (state_badge(&r.state))
                    strong { (r.author) }
                    span.muted { " wants to land " (d.stack.len()) " changeset" @if d.stack.len() != 1 { "s" } " into " }
                    a.branch href=(url(&ctx.name, "tree", &target, None)) { (icon("branch")) (target) }
                    span.muted { " from " }
                    a.sha href=(url(&ctx.name, "commit", &r.head.to_hex(), None)) { (short(&r.head)) }
                }
            }
            div.filter-tabs.under {
                a.active[!files_tab] href=(base_url) { (icon("review")) "Conversation" }
                a.active[files_tab] href=(format!("{base_url}/files")) { (icon("code")) "Changes" span.count { (d.stack.len()) } }
            }
            @if let Some(files) = &files {
                (diff_view(ctx, &r.head.to_hex(), files))
            } @else {
                div.review-grid {
                    div.timeline {
                        div.card.comment {
                            div.comment-head { (avatar(&r.author, "", false)) strong { (r.author) } span.muted { " opened " (time(r.created_ms)) } }
                            div.comment-body {
                                @if r.body.trim().is_empty() { p.muted { "No description." } }
                                @else { article.markdown { (PreEscaped(markdown::render(&r.body, None))) } }
                            }
                        }
                        div.card {
                            div.card-head { (icon("commit")) "Changesets" }
                            ul.commits { @for l in &d.stack { (commit_row(ctx, l, false)) } }
                        }
                        @for e in &d.events {
                            @match e.kind.as_str() {
                                "comment" => {
                                    div.card.comment {
                                        div.comment-head { (avatar(&e.author, "", false)) strong { (e.author) } span.muted { " commented " (time(e.ts_ms)) } }
                                        div.comment-body { article.markdown { (PreEscaped(markdown::render(&e.body, None))) } }
                                    }
                                }
                                "opened" => {}
                                kind => {
                                    div.event {
                                        span class=(format!("event-icon {kind}")) { (icon(match kind { "landed" => "merge", "approved" => "check", "closed" => "x", "updated" => "commit", _ => "review" })) }
                                        strong { (e.author) }
                                        span.muted {
                                            @match kind {
                                                "landed" => { " landed this " }
                                                "approved" => { " approved " }
                                                "closed" => { " closed this " }
                                                "reopened" => { " reopened this " }
                                                "updated" => { " pushed a new head " }
                                                k => { " " (k) " " }
                                            }
                                            (time(e.ts_ms))
                                        }
                                        @if let Some(c) = e.cs { @if kind == "updated" { " " a.sha href=(url(&ctx.name, "commit", &c.to_hex(), None)) { (short(&c)) } } }
                                        @if !e.body.is_empty() && kind != "approved" { span.muted { " — " (e.body) } }
                                        @if kind == "approved" && !e.body.is_empty() { div.event-body { (e.body) } }
                                    }
                                }
                            }
                        }
                        @if r.state == "open" {
                            div.card.land-box {
                                @match status {
                                    Some((true, _, _)) => { div.land-status.ok { (icon("check")) div { strong { "Ready to land" } div.muted.small { "Fast-forward: " (target) " is an ancestor of the head." } } } }
                                    Some((_, 0, _)) => { div.land-status.ok { (icon("check")) div { strong { "Nothing to land" } div.muted.small { "Every changeset here is already in " (target) ". You can close this review." } } } }
                                    Some((false, ahead, behind)) => { div.land-status.warn { (icon("merge")) div { strong { "Will rebase on land" } div.muted.small { (ahead) " ahead, " (behind) " behind " (target) ". flux re-applies the stack server-side and three-way merges overlapping files; real conflicts are reported." } } } }
                                    None => { div.land-status.ok { (icon("check")) div { strong { "Ready to land" } div.muted.small { (target) " doesn't exist yet; landing creates it." } } } }
                                }
                                @if !approvals.is_empty() { div.muted.small { (icon("check")) " Approved by " (approvals.join(", ")) } }
                                @if can_write && !matches!(status, Some((_, 0, _))) {
                                    form method="post" action=(format!("{base_url}/land")) {
                                        button.btn.primary.big type="submit" { (icon("merge")) "Land into " (target) }
                                    }
                                }
                            }
                        }
                        @if can_write {
                            form.card.comment-form method="post" action=(format!("{base_url}/comment")) {
                                textarea name="body" rows="4" placeholder="Leave a comment (Markdown)" required {}
                                div.form-actions {
                                    @if r.state == "open" {
                                        button.btn type="submit" formaction=(format!("{base_url}/close")) formnovalidate { "Close" }
                                        button.btn type="submit" formaction=(format!("{base_url}/approve")) formnovalidate { (icon("check")) "Approve" }
                                    }
                                    @if r.state == "closed" {
                                        button.btn type="submit" formaction=(format!("{base_url}/reopen")) formnovalidate { "Reopen" }
                                    }
                                    button.btn.primary type="submit" { "Comment" }
                                }
                            }
                        } @else {
                            div.notice.small { a href=(format!("/-/login?next={}", enc(&req.path))) { "Sign in" } " to comment or land." }
                        }
                    }
                    aside.review-side {
                        div.side-block { div.side-title { "Target" } code { (target) } }
                        div.side-block { div.side-title { "Head" } a.sha href=(url(&ctx.name, "commit", &r.head.to_hex(), None)) { (short(&r.head)) } }
                        @if let Some(l) = r.landed { div.side-block { div.side-title { "Landed as" } a.sha href=(url(&ctx.name, "commit", &l.to_hex(), None)) { (short(&l)) } } }
                        div.side-block { div.side-title { "Approvals" } @if approvals.is_empty() { span.muted { "None yet" } } @for a in &approvals { div { (avatar(a, "", false)) " " (a) } } }
                        div.side-block.muted.small { "Land from the CLI: " code { "flux review land " (r.number) } }
                    }
                }
            }
        }
    };
    let n = nav(web, ctx, Tab::Reviews, &target).await;
    Ok(page(web, req, &format!("{} · Review #{}", r.title, r.number), Some(n), body))
}

#[derive(Deserialize, Default)]
pub struct ReviewForm {
    #[serde(default)]
    title: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    head: String,
    #[serde(default)]
    target: String,
}

pub async fn repo_post(
    State(web): State<WebState>,
    req: Req,
    headers: HeaderMap,
    Path(rest): Path<String>,
    Form(f): Form<ReviewForm>,
) -> WebResult {
    check_origin(&headers)?;
    let (repo, segs) = split_repo(&rest);
    let ctx = repo_ctx(&web, &req, &repo).await?;
    req.principal.require_write()?;
    let actor = req.principal.name().to_string();
    let flux = &web.flux;
    let segs: Vec<&str> = segs.iter().map(String::as_str).collect();
    match segs.as_slice() {
        ["reviews"] => {
            let head = flux.resolve_rev(ctx.id, f.head.trim()).await?;
            let target = if f.target.trim().is_empty() {
                ctx.default_branch().unwrap_or("main").to_string()
            } else {
                f.target.trim().to_string()
            };
            let r = flux.create_review(ctx.id, &f.title, &f.body, &actor, head, target.as_bytes()).await?;
            Ok(Redirect::to(&format!("/{}/-/reviews/{}", enc(&ctx.name), r.number)).into_response())
        }
        ["reviews", n, "land"] => {
            let n = parse_num(n)?;
            flux.land_review(ctx.id, n, &actor).await?;
            Ok(Redirect::to(&format!("/{}/-/reviews/{n}", enc(&ctx.name))).into_response())
        }
        ["reviews", n, action @ ("comment" | "approve" | "close" | "reopen")] => {
            let n = parse_num(n)?;
            flux.update_review(ctx.id, n, action, &actor, &f.body, None).await?;
            Ok(Redirect::to(&format!("/{}/-/reviews/{n}", enc(&ctx.name))).into_response())
        }
        _ => Err(WebError::NotFound("no such action".into())),
    }
}
