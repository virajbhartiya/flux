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
use super::{Req, WebError, WebResult, WebState};
use crate::core::{Flux, Loaded};
use crate::rbac::Role;

const PAGE_SIZE: usize = 50;
/// Diff lines rendered inline on commit and compare pages; later files
/// load on demand, so huge diffs still open instantly.
const DIFF_LINE_BUDGET: usize = 2_500;
/// Reviews render more inline (comments attach to rendered lines).
const REVIEW_LINE_BUDGET: usize = 12_000;
const DEFERRED_NOTE: &str = "Diff not shown to keep this page fast.";
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
    config: Arc<crate::repo_config::RepoConfig>,
    /// The viewer's role on this repository.
    role: Role,
}

impl RepoCtx {
    fn require(&self, need: Role) -> WebResult<()> {
        if self.role >= need {
            Ok(())
        } else {
            Err(WebError::Forbidden(format!("this needs {} access to {}", need.as_str(), self.name)))
        }
    }

    fn can(&self, need: Role) -> bool {
        self.role >= need
    }

    fn default_branch(&self) -> Option<&str> {
        if let Some(b) = self.config.default_branch.as_deref()
            && self.is_ref(b)
        {
            return Some(b);
        }
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
    let id = web.flux.repo(name).await?;
    let role = web.flux.authorize(&req.principal, id, Role::Read).await?;
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
    let config = web.flux.repo_config(id).await?;
    Ok(RepoCtx { name: name.to_string(), id, refs, config, role })
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
    let repos = web.flux.readable_repos(&req.principal).await?;
    let mut cards = Vec::new();
    let mut feed: Vec<(String, flux_store::RefLogRow)> = Vec::new();
    for r in &repos {
        let refs = web.flux.meta.list_refs(r.id, b"", 1000).await?;
        let cfg = web.flux.repo_config(r.id).await?;
        let head = refs
            .iter()
            .find(|(n, _)| cfg.default_branch.as_deref().is_some_and(|d| n == d.as_bytes()))
            .or_else(|| refs.iter().find(|(n, _)| n == b"main" || n == b"master"))
            .or_else(|| refs.first())
            .map(|(n, v)| (String::from_utf8_lossy(n).into_owned(), v.target));
        let latest = match &head {
            Some((_, id)) => web.flux.load_one_arc(r.id, id).await.ok(),
            None => None,
        };
        for row in web.flux.meta.ref_log(r.id, None, 8).await? {
            feed.push((r.name.clone(), row));
        }
        cards.push((r.name.clone(), cfg.description.clone(), refs.len(), head, latest));
    }
    cards.sort_by_key(|c| std::cmp::Reverse(c.4.as_ref().map(|l| l.changeset.author_time.unix_ms).unwrap_or(0)));
    feed.sort_by_key(|f| std::cmp::Reverse(f.1.ts_ms));
    feed.truncate(14);
    let who = req.principal.user.as_ref().map(|u| u.name.clone());
    let body = html! {
        div.container.wide {
            div.hero {
                h1 { @match &who { Some(u) => { "Welcome back, " (u) } None => { "Repositories" } } }
                p { (repos.len()) " repositor" @if repos.len() == 1 { "y" } @else { "ies" } " · press " kbd { "⌘" } kbd { "K" } " to jump anywhere" }
            }
            div.split-layout {
                div {
                    div.page-head.row {
                        h2 { "All repositories" }
                        div.spacer {}
                        @if req.principal.can_write() {
                            a.btn.primary href="/-/new" { (icon("plus")) "New repository" }
                        }
                    }
                    @if cards.is_empty() {
                        (empty_state("No repositories yet", html! {
                            p { "Create one above, then import history or start committing:" }
                            pre.cmd { "flux-import-git --server " (req.origin) " --repo demo import /path/to/repo.git --create" }
                        }))
                    }
                    div.repo-grid.reveal {
                        @for (name, desc, nrefs, head, latest) in &cards {
                            a.repo-card href=(view::repo_url(name)) {
                                div.repo-card-title { (icon("repo")) (name) }
                                div.repo-card-desc { @if desc.is_empty() { span.faint { "No description" } } @else { (desc) } }
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
                aside.side {
                    section {
                        h3 { "Latest activity" }
                        @if feed.is_empty() { p.muted.small { "Nothing yet." } }
                        ul.feed {
                            @for (repo, r) in &feed {
                                li {
                                    (avatar(&r.actor, "", false))
                                    div.what {
                                        strong { (r.actor) }
                                        @match (r.old, r.new) { (None, Some(_)) => { " created " } (Some(_), None) => { " deleted " } _ => { " moved " } }
                                        code { (String::from_utf8_lossy(&r.name)) }
                                        " in " a.link href=(view::repo_url(repo)) { (repo) }
                                        div.muted { (r.reason) " · " (time(r.ts_ms)) }
                                    }
                                }
                            }
                        }
                    }
                    section {
                        h3 { "Get started" }
                        p.muted.small { "Work in a sparse checkout:" }
                        pre.cmd { "flux attach " (req.origin) " REPO" }
                        p.muted.small { "Or use plain git:" }
                        pre.cmd { "git clone " (req.origin) "/REPO.git" }
                    }
                }
            }
        }
    };
    Ok(page(&web, &req, "Home", None, body))
}

pub async fn repos_json(State(web): State<WebState>, req: Req) -> WebResult {
    let repos: Vec<String> = web.flux.readable_repos(&req.principal).await?.into_iter().map(|r| r.name).collect();
    Ok(json_response(serde_json::json!({ "repos": repos }), "private, max-age=10"))
}

fn json_response(v: serde_json::Value, cache: &'static str) -> Response {
    let mut r = v.to_string().into_response();
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    r
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

use super::account::check_origin;

// ───────────── repo dispatch ─────────────

pub fn decode_repo(s: &str) -> String {
    url_decode(s)
}

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
    // `/{name}` with no such repository: a user or organization page.
    if segs.is_empty()
        && !repo.contains('/')
        && web.flux.repo(&repo).await.is_err()
        && let Some(r) = super::account::owner_page(&web, &req, &repo).await?
    {
        return Ok(r);
    }
    let ctx = repo_ctx(&web, &req, &repo).await?;
    let Some((action, args)) = segs.split_first() else {
        let rev = ctx.default_branch().map(str::to_string);
        return match rev {
            Some(r) => tree(&web, &req, &ctx, &r, None).await,
            None => empty_repo(&web, &req, &ctx).await,
        };
    };
    let q = query_map(&req.query);
    // Path permissions (SPEC §12.2): restricted paths look like they don't exist.
    if ctx.config.has_rules() && matches!(action.as_str(), "tree" | "blob" | "raw" | "blame" | "commits" | "edit" | "new") {
        let (_, rest) = ctx.split_rev(args);
        if let Some(p) = path_from(rest)? {
            let ok = if action == "tree" || action == "new" || action == "commits" {
                ctx.config.dir_visible(&req.principal, p.as_bytes())
            } else {
                ctx.config.allows(&req.principal, p.as_bytes(), false)
            };
            if !ok {
                return Err(WebError::NotFound(format!("{p} does not exist")));
            }
        }
    }
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
            files_json(&web, &req, &ctx, &rev).await
        }
        "events" => Ok(events(&web, &ctx)),
        "refs.json" => {
            let names: Vec<&str> = ctx.refs.iter().map(|(n, _)| n.as_str()).collect();
            Ok(json_response(serde_json::json!({ "refs": names }), "private, max-age=5"))
        }
        "settings" if args.first().map(String::as_str) == Some("access") => settings_access(&web, &req, &ctx).await,
        "difffile" => diff_file_fragment(&web, &req, &ctx, args, &q).await,
        "settings" => settings(&web, &req, &ctx).await,
        "edit" => {
            let (rev, rest) = ctx.split_rev(args);
            let path = path_from(rest)?.ok_or_else(|| WebError::BadRequest("missing path".into()))?;
            edit_form(&web, &req, &ctx, &rev, Some(&path), None).await
        }
        "new" => {
            let (rev, rest) = ctx.split_rev(args);
            let dir = path_from(rest)?;
            edit_form(&web, &req, &ctx, &rev, None, dir.as_ref()).await
        }
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

async fn nav<'a>(web: &WebState, req: &Req, ctx: &'a RepoCtx, tab: Tab, rev: &'a str) -> RepoNav<'a> {
    let changesets = match web.flux.repo_graph(ctx.id).await {
        Ok(g) => g.graph.read().unwrap().len() as u64,
        Err(_) => 0,
    };
    RepoNav {
        repo: &ctx.name,
        tab,
        rev,
        open_reviews: open_reviews(&web.flux, ctx.id).await,
        description: ctx.config.description.clone(),
        default_branch: ctx.default_branch().unwrap_or("main").to_string(),
        changesets,
        branches: ctx.refs.len(),
        origin: req.origin.clone(),
        is_admin: ctx.can(Role::Maintain),
    }
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
    let n = nav(web, req, ctx, Tab::Code, "main").await;
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
    let visible = |name: &[u8]| {
        let full: Vec<u8> = match &path {
            Some(p) => [p.as_bytes(), b"/", name].concat(),
            None => name.to_vec(),
        };
        ctx.config.dir_visible(&req.principal, &full)
    };
    let mut entries: Vec<&TreeEntry> = t.entries().iter().filter(|e| visible(e.name.as_bytes())).collect();
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
    let main_col = html! {
        div.toolbar {
            (ref_picker(ctx, rev, "tree", path.as_ref()))
            (breadcrumbs(&ctx.name, rev, path.as_ref()))
            div.spacer {}
            span.muted.small.hide-sm { (view::count(total_files)) " files · " (view::size(t.total_size())) }
            button.btn.small type="button" data-palette-files title="Go to file (t)" { (icon("file")) "Go to file" kbd { "t" } }
            @if ctx.can(Role::Write) {
                a.btn.small href=(url(&ctx.name, "new", rev, path.as_ref())) { (icon("plus")) "Add file" }
            }
        }
        div.card.files {
            @match latest_id.and_then(|id| loaded.get(&id)) {
                Some(l) => (commit_bar(ctx, l, rev, &history_url)),
                None => div.commit-bar.muted { span.skeleton {} },
            }
            table.file-table {
                tbody.reveal {
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
    };
    let body = if path.is_none() {
        let side = repo_sidebar(web, ctx, rev, &cs, root, t.total_size(), total_files).await?;
        html! { div.container.wide { div.split-layout { div { (main_col) } (side) } } }
    } else {
        html! { div.container { (main_col) } }
    };
    let title = match &path {
        Some(p) => format!("{p} at {} · {}", rev_label(rev), ctx.name),
        None => ctx.name.clone(),
    };
    let n = nav(web, req, ctx, Tab::Code, rev).await;
    Ok(page(web, req, &title, Some(n), body))
}

/// The repository home sidebar: about, languages, contributors, activity
/// and open reviews.
async fn repo_sidebar(
    web: &WebState,
    ctx: &RepoCtx,
    rev: &str,
    cs: &ChangesetId,
    root: TreeId,
    total_size: u64,
    total_files: u64,
) -> WebResult<Markup> {
    let flux = &web.flux;
    let langs = match web.langs.get(&root) {
        Some(l) => l,
        None => {
            let files = flux.file_list(root).await?;
            let l = Arc::new(super::lang::breakdown(&files));
            web.langs.insert(root, l.clone());
            l
        }
    };
    // Contributors to the last 300 changes.
    let recent = flux.log_page(ctx.id, cs, &[], None, true, 0, 300).await?;
    let mut authors: Vec<(String, String, usize)> = Vec::new();
    for l in &recent {
        let (n, e) = person(&l.changeset.author);
        match authors.iter_mut().find(|a| a.0 == n) {
            Some(a) => a.2 += 1,
            None => authors.push((n, e, 1)),
        }
    }
    authors.sort_by_key(|a| std::cmp::Reverse(a.2));
    let log = flux.meta.ref_log(ctx.id, None, 6).await?;
    let reviews = flux.meta.list_reviews(ctx.id, Some("open"), 5).await?;
    let mut offsets = Vec::with_capacity(langs.len());
    let mut x = 0.0f64;
    for (_, _, pct) in langs.iter() {
        offsets.push(x);
        x += pct;
    }
    Ok(html! {
        aside.side {
            section {
                h3 { "About" }
                @if !ctx.config.description.is_empty() { p { (ctx.config.description) } }
                p.muted.small { (view::count(total_files)) " files · " (view::size(total_size)) " at " code { (rev_label(rev)) } }
            }
            @if !langs.is_empty() {
                section {
                    h3 { "Languages" }
                    svg.langbar viewBox="0 0 100 8" preserveAspectRatio="none" width="100%" height="8" aria-hidden="true" {
                        @for ((_, color, pct), x) in langs.iter().zip(&offsets) {
                            rect x=(format!("{x:.3}")) y="0" width=(format!("{:.3}", (pct - 0.4).max(0.3))) height="8" fill=(color) {}
                        }
                    }
                    ul.langs {
                        @for (name, color, pct) in langs.iter() {
                            li { svg width="9" height="9" aria-hidden="true" { circle cx="4.5" cy="4.5" r="4.5" fill=(color) {} } b { (name) } span.faint { (format!("{pct:.1}%")) } }
                        }
                    }
                }
            }
            @if !authors.is_empty() {
                section {
                    h3 { "Contributors" span.faint { " · recent" } }
                    div.contributors {
                        @for (n, e, c) in authors.iter().take(18) {
                            span title=(format!("{n} · {c} of the last {} changes", recent.len())) { (avatar(n, e, true)) }
                        }
                    }
                }
            }
            @if !reviews.is_empty() {
                section {
                    h3 { "Open reviews" }
                    ul.feed {
                        @for r in &reviews {
                            li { (icon("review")) div.what { a href=(format!("/{}/-/reviews/{}", enc(&ctx.name), r.number)) { (r.title) } div.muted { "#" (r.number) " by " (r.author) " · " (time(r.updated_ms)) } } }
                        }
                    }
                }
            }
            @if !log.is_empty() {
                section {
                    h3 { "Activity" }
                    ul.feed {
                        @for r in &log {
                            li {
                                (avatar(&r.actor, "", false))
                                div.what {
                                    strong { (r.actor) } " moved " code { (String::from_utf8_lossy(&r.name)) }
                                    div.muted { (r.reason) " · " (time(r.ts_ms)) }
                                }
                            }
                        }
                    }
                    a.link.small href=(format!("/{}/-/activity", enc(&ctx.name))) { "All activity →" }
                }
            }
        }
    })
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
    let mut editable = false;
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
                editable = entry.size <= 1 << 20 && entry.file_kind().is_some_and(FileKind::is_plain);
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
                    @if ctx.can(Role::Write) && editable {
                        a.btn.small href=(url(&ctx.name, "edit", rev, Some(path))) { (icon("edit")) "Edit" }
                    }
                    a.btn.small href=(url(&ctx.name, "blame", rev, Some(path))) { (icon("blame")) "Blame" }
                    button.btn.small.copy-btn type="button" data-copy-url=(permalink) title="Copy permalink (y)" { (icon("copy")) "Permalink" }
                }
                (content_block)
            }
            @if hl_pending { div.hl-pending hidden {} }
        }
    };
    let n = nav(web, req, ctx, Tab::Code, rev).await;
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
    let n = nav(web, req, ctx, Tab::Code, rev).await;
    Ok(page(web, req, &format!("Blame {path} · {}", ctx.name), Some(n), body))
}

// ───────────── history ─────────────

fn commit_row(ctx: &RepoCtx, l: &Loaded, rev_for_tree: bool) -> Markup {
    commit_row_with(ctx, l, rev_for_tree, None)
}

fn commit_row_with(ctx: &RepoCtx, l: &Loaded, rev_for_tree: bool, graph: Option<Markup>) -> Markup {
    let (n, e) = person(&l.changeset.author);
    let id = l.id.to_hex();
    html! {
        li.commit {
            @if let Some(g) = graph { (g) }
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

const LANE_COLORS: [&str; 8] = ["#3466d6", "#52a7a0", "#a580c4", "#c4a14f", "#6aab7b", "#cc7a75", "#5c9fd6", "#8b929c"];
const ROW_H: f64 = 60.0;
const LANE_W: f64 = 13.0;
/// Lanes beyond this are not drawn (busy histories stay readable).
const MAX_LANES: usize = 6;

/// History graph: one small SVG per row. Lanes hold the changeset each
/// column is waiting for; a node consumes its lane and hands it to its
/// first parent, opening (or joining) lanes for other parents.
fn graph_rows(items: &[Arc<Loaded>]) -> Vec<Markup> {
    let mut lanes: Vec<Option<ChangesetId>> = Vec::new();
    struct Row {
        col: usize,
        before: Vec<Option<ChangesetId>>,
        after: Vec<Option<ChangesetId>>,
        id: ChangesetId,
        parents: Vec<ChangesetId>,
    }
    let mut rows = Vec::with_capacity(items.len());
    let mut width = 1;
    for l in items {
        let id = l.id;
        let col = match lanes.iter().position(|x| *x == Some(id)) {
            Some(c) => c,
            None => match lanes.iter().position(Option::is_none) {
                Some(free) => free,
                None => {
                    lanes.push(None);
                    lanes.len() - 1
                }
            },
        };
        let before = lanes.clone();
        // Every lane waiting for this node ends here.
        for slot in lanes.iter_mut() {
            if *slot == Some(id) {
                *slot = None;
            }
        }
        let parents = l.changeset.parents.clone();
        if let Some(p0) = parents.first() {
            if lanes.contains(&Some(*p0)) {
                // Another lane already continues to this parent.
            } else {
                lanes[col] = Some(*p0);
            }
        }
        for p in parents.iter().skip(1) {
            if !lanes.contains(&Some(*p)) {
                match lanes.iter().position(Option::is_none) {
                    Some(free) => lanes[free] = Some(*p),
                    None => lanes.push(Some(*p)),
                }
            }
        }
        while lanes.last() == Some(&None) {
            lanes.pop();
        }
        width = width.max(before.len()).max(lanes.len()).max(col + 1);
        rows.push(Row { col, before, after: lanes.clone(), id, parents });
    }
    let width = width.min(MAX_LANES);
    let x = |i: usize| 9.0 + i as f64 * LANE_W;
    let w = x(width.saturating_sub(1)) + 9.0;
    let shown = |i: usize| i < MAX_LANES;
    let color = |i: usize| LANE_COLORS[i % LANE_COLORS.len()];
    let curve = |x1: f64, y1: f64, x2: f64, y2: f64| {
        if (x1 - x2).abs() < 0.1 {
            format!("M{x1} {y1}V{y2}")
        } else {
            let my = (y1 + y2) / 2.0;
            format!("M{x1} {y1}C{x1} {my},{x2} {my},{x2} {y2}")
        }
    };
    rows.iter()
        .map(|r| {
            let mid = ROW_H / 2.0;
            let mut paths: Vec<(String, &str)> = Vec::new();
            for (j, slot) in r.before.iter().enumerate() {
                match slot {
                    Some(v) if *v == r.id && shown(j) && shown(r.col) => {
                        paths.push((curve(x(j), 0.0, x(r.col), mid), color(j)));
                    }
                    Some(v) if *v == r.id => {}
                    Some(v) => {
                        if let Some(k) = r.after.iter().position(|a| a == &Some(*v))
                            && shown(j)
                            && shown(k)
                        {
                            paths.push((curve(x(j), 0.0, x(k), ROW_H), color(j)));
                        }
                    }
                    None => {}
                }
            }
            for p in &r.parents {
                if let Some(k) = r.after.iter().position(|a| a == &Some(*p))
                    && shown(k)
                    && shown(r.col)
                {
                    paths.push((curve(x(r.col), mid, x(k), ROW_H), color(k)));
                }
            }
            let col = r.col.min(MAX_LANES - 1);
            let merge = r.parents.len() > 1;
            html! {
                svg.graph width=(format!("{w}")) height=(format!("{ROW_H}")) viewBox=(format!("0 0 {w} {ROW_H}")) aria-hidden="true" {
                    @for (d, c) in &paths { path d=(d) stroke=(c) {} }
                    circle cx=(format!("{}", x(col))) cy=(format!("{mid}")) r=(if merge { "4" } else { "4.5" }) class=[merge.then_some("merge-node")] fill=(color(col)) stroke=(color(col)) {}
                }
            }
        })
        .collect()
}

fn commit_list(ctx: &RepoCtx, items: &[Arc<Loaded>]) -> Markup {
    commit_list_inner(ctx, items, false)
}

fn commit_list_graph(ctx: &RepoCtx, items: &[Arc<Loaded>]) -> Markup {
    commit_list_inner(ctx, items, true)
}

fn commit_list_inner(ctx: &RepoCtx, items: &[Arc<Loaded>], graph: bool) -> Markup {
    let mut graphs: Vec<Option<Markup>> =
        if graph { graph_rows(items).into_iter().map(Some).collect() } else { vec![None; items.len()] };
    let mut groups: Vec<(i64, Vec<(&Arc<Loaded>, Option<Markup>)>)> = Vec::new();
    for (l, g) in items.iter().zip(graphs.drain(..)) {
        let d = view::day_key(l.changeset.author_time.unix_ms);
        match groups.last_mut() {
            Some((k, v)) if *k == d => v.push((l, g)),
            _ => groups.push((d, vec![(l, g)])),
        }
    }
    html! {
        @for (_, group) in groups {
            div.day {
                div.day-label { (view::day_label(group[0].0.changeset.author_time.unix_ms)) }
                ul.commits.card { @for (l, g) in group { (commit_row_with(ctx, l, true, g)) } }
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
            (if path.is_none() { commit_list_graph(ctx, items) } else { commit_list(ctx, items) })
            div.pager {
                @if page_no > 0 { a.btn href=(format!("{base}?page={}", page_no - 1)) { "← Newer" } }
                @if more { a.btn href=(format!("{base}?page={}", page_no + 1)) { "Older →" } }
            }
        }
    };
    let n = nav(web, req, ctx, Tab::Commits, rev).await;
    Ok(page(web, req, &format!("Changes · {}", ctx.name), Some(n), body))
}

#[derive(Clone)]
pub(super) struct RenderedFile {
    path: RepoPath,
    status: &'static str,
    old: Option<flux_derive::TreeFile>,
    new: Option<flux_derive::TreeFile>,
    diff: Option<FileDiff>,
    note: Option<String>,
    old_hl: Option<Arc<Vec<String>>>,
    new_hl: Option<Arc<Vec<String>>>,
}

/// Diff two trees into renderable file diffs, fetching contents in
/// parallel and stopping detailed diffs once the line budget is spent.
async fn diff_files(web: &WebState, old: Option<TreeId>, new: Option<TreeId>) -> WebResult<Vec<RenderedFile>> {
    diff_files_with(web, old, new, None, DIFF_LINE_BUDGET).await
}

/// `only`: restrict to one path. `line_budget`: lines diffed in full.
async fn diff_files_with(
    web: &WebState,
    old: Option<TreeId>,
    new: Option<TreeId>,
    only: Option<&RepoPath>,
    line_budget: usize,
) -> WebResult<Vec<RenderedFile>> {
    let filter = |p: &RepoPath, is_dir: bool| match only {
        None => true,
        Some(t) => {
            let (t, p) = (t.as_bytes(), p.as_bytes());
            if is_dir { t.starts_with(p) && t.get(p.len()) == Some(&b'/') } else { t == p }
        }
    };
    // Trees are immutable: a finished (fully highlighted) result is reusable.
    let key = (old, new, only.cloned(), line_budget);
    if let Some(hit) = web.diffs.get(&key) {
        return Ok((*hit).clone());
    }
    let entries = diff_trees(&web.flux.blobs, old, new, &filter).await?;
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
    let mut budget = line_budget;
    let mut out = Vec::with_capacity(entries.len());
    let mut hl_jobs: Vec<(usize, bool, flux_core::ContentId, String, String)> = Vec::new();
    for (i, e) in entries.into_iter().enumerate() {
        let status = match (&e.old, &e.new) {
            (None, Some(_)) => "added",
            (Some(_), None) => "deleted",
            _ => "modified",
        };
        let mut rf = RenderedFile {
            path: e.path.clone(),
            status,
            old: e.old.clone(),
            new: e.new.clone(),
            diff: None,
            note: None,
            old_hl: None,
            new_hl: None,
        };
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
            rf.note = Some(DEFERRED_NOTE.into());
            // Still count lines.
            let d = diff_text(&String::from_utf8_lossy(&a_bytes), &String::from_utf8_lossy(&b_bytes), 0);
            rf.diff = Some(FileDiff { hunks: vec![], ..d });
        } else {
            let d = diff_text(&String::from_utf8_lossy(&a_bytes), &String::from_utf8_lossy(&b_bytes), 3);
            let n: usize = d.hunks.iter().map(|h| h.lines.len()).sum();
            budget = budget.saturating_sub(n);
            rf.diff = Some(d);
            // Highlight both sides (cached by content id; bounded below).
            let p = e.path.to_string();
            if let Some((id, _)) = a {
                hl_jobs.push((out.len(), false, *id, p.clone(), String::from_utf8_lossy(&a_bytes).into_owned()));
            }
            if let Some((id, _)) = b {
                hl_jobs.push((out.len(), true, *id, p, String::from_utf8_lossy(&b_bytes).into_owned()));
            }
        }
        out.push(rf);
    }
    // Run highlighting concurrently with an overall time budget; files that
    // aren't done render plain now (and come back highlighted next time).
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(120);
    let mut tasks = Vec::new();
    for (idx, new_side, id, path, text) in hl_jobs {
        if let Some(v) = web.hl.cached(id, &path, &text) {
            if new_side {
                out[idx].new_hl = Some(v)
            } else {
                out[idx].old_hl = Some(v)
            }
            continue;
        }
        if !web.hl.has_grammar(&path, &text) {
            continue;
        }
        let w = web.clone();
        tasks.push((idx, new_side, tokio::task::spawn_blocking(move || w.hl.lines(id, &path, &text))));
    }
    let mut complete = true;
    for (idx, new_side, task) in tasks {
        match tokio::time::timeout_at(deadline, task).await {
            Ok(Ok(Some(v))) => {
                if new_side {
                    out[idx].new_hl = Some(v)
                } else {
                    out[idx].old_hl = Some(v)
                }
            }
            Ok(_) => {}
            Err(_) => complete = false,
        }
    }
    if complete {
        web.diffs.insert(key, Arc::new(out.clone()));
    }
    Ok(out)
}

/// HTML for each diff line: syntax highlighted when available, with
/// word-level emphasis on lines that pair up within a change block.
fn diff_line_html(f: &RenderedFile, h: &super::diff::Hunk) -> Vec<String> {
    use super::diff::{escape_html, word_ranges, wrap_ranges};
    let base = |l: &super::diff::Line| -> String {
        let hl = match l.kind {
            LineKind::Del => l.old.and_then(|n| f.old_hl.as_ref().and_then(|v| v.get(n as usize - 1).cloned())),
            _ => l.new.and_then(|n| f.new_hl.as_ref().and_then(|v| v.get(n as usize - 1).cloned())),
        };
        hl.unwrap_or_else(|| escape_html(&l.text))
    };
    let mut html: Vec<String> = h.lines.iter().map(base).collect();
    let mut i = 0;
    while i < h.lines.len() {
        if h.lines[i].kind != LineKind::Del {
            i += 1;
            continue;
        }
        let ds = i;
        while i < h.lines.len() && h.lines[i].kind == LineKind::Del {
            i += 1;
        }
        let as_ = i;
        while i < h.lines.len() && h.lines[i].kind == LineKind::Add {
            i += 1;
        }
        let (nd, na) = (as_ - ds, i - as_);
        if na == 0 || nd.max(na) > 3 * nd.min(na) {
            continue;
        }
        for k in 0..nd.min(na) {
            if let Some((ra, rb)) = word_ranges(&h.lines[ds + k].text, &h.lines[as_ + k].text) {
                html[ds + k] = wrap_ranges(&html[ds + k], &ra);
                html[as_ + k] = wrap_ranges(&html[as_ + k], &rb);
            }
        }
    }
    html
}

/// Inline review comments rendered into a diff.
struct Comments<'a> {
    /// POST target for new comments.
    action: String,
    can_write: bool,
    /// (path, line) → comments. Positive lines are new-side line numbers,
    /// negative ones old-side.
    threads: HashMap<(String, i64), Vec<&'a flux_store::ReviewEvent>>,
}

fn thread_row(c: &Comments<'_>, path: &str, line: i64, events: &[&flux_store::ReviewEvent]) -> Markup {
    html! {
        tr.thread {
            td colspan="3" {
                div.thread-box {
                    @for e in events {
                        div.c {
                            div.c-head { (avatar(&e.author, "", false)) strong { (e.author) } span.faint { (time(e.ts_ms)) } }
                            div.markdown { (PreEscaped(markdown::render(&e.body, None))) }
                        }
                    }
                    @if c.can_write {
                        form method="post" action=(c.action) {
                            input type="hidden" name="path" value=(path);
                            input type="hidden" name="line" value=(line);
                            textarea name="body" rows="2" placeholder="Reply…" required {}
                            div.form-actions { button.btn.small.primary type="submit" { "Reply" } }
                        }
                    }
                }
            }
        }
    }
}

/// `lazy`: `(base, head)` changesets; deferred files load from
/// `-/difffile/{base}/{head}?path=…` when they come into view.
fn diff_view(ctx: &RepoCtx, rev: &str, files: &[RenderedFile], lazy: Option<(&str, &str)>) -> Markup {
    diff_view_with(ctx, rev, files, None, lazy)
}

fn diff_view_with(
    ctx: &RepoCtx,
    rev: &str,
    files: &[RenderedFile],
    comments: Option<&Comments<'_>>,
    lazy: Option<(&str, &str)>,
) -> Markup {
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
                @if let (Some(note), Some((a, b))) = (&f.note, lazy) {
                    @if note == DEFERRED_NOTE {
                        div.lazy-diff data-src=(format!("/{}/-/difffile/{a}/{b}?path={}", enc(&ctx.name), enc(&f.path.to_string()))) {
                            button.btn.small type="button" { "Load diff" }
                        }
                    } @else {
                        div.notice.small { (note) }
                    }
                } @else if let Some(note) = &f.note {
                    div.notice.small { (note) }
                }
                @if let Some(d) = &f.diff {
                    @if !d.hunks.is_empty() {
                        (diff_table(f, d, comments))
                    } @else if f.note.is_none() && f.status == "modified" {
                        div.notice.small { "File mode changed; contents are identical." }
                    }
                }
            }
        }
    }
}

fn diff_table(f: &RenderedFile, d: &FileDiff, comments: Option<&Comments<'_>>) -> Markup {
    html! {
                        @let path_s = f.path.to_string();
                        @let commentable = comments.is_some_and(|c| c.can_write);
                        table.diff.commentable[commentable] data-comment-url=[comments.map(|c| c.action.clone())] data-path=(path_s) {
                            @for h in &d.hunks {
                                @let lines_html = diff_line_html(f, h);
                                tbody {
                                    tr.hunk { td.ln {} td.ln {} td.lc { "@@ -" (h.old_start) " +" (h.new_start) " @@" } }
                                    @for (l, lh) in h.lines.iter().zip(&lines_html) {
                                        @let key = if l.kind == LineKind::Del { -(l.old.unwrap_or(0) as i64) } else { l.new.unwrap_or(0) as i64 };
                                        tr class=(match l.kind { LineKind::Add => "l-add", LineKind::Del => "l-del", LineKind::Context => "l-ctx" }) {
                                            td.ln data-line=[l.old] {}
                                            td.ln data-line=[l.new] {}
                                            td.lc {
                                                @if commentable { button.add-comment type="button" data-line=(key) title="Comment on this line" { "+" } }
                                                span.mark { (match l.kind { LineKind::Add => "+", LineKind::Del => "-", LineKind::Context => " " }) } (PreEscaped(lh))
                                            }
                                        }
                                        @if let Some(c) = comments {
                                            @if let Some(t) = c.threads.get(&(path_s.clone(), key)) { (thread_row(c, &path_s, key, t)) }
                                        }
                                    }
                                }
                            }
                        }
    }
}

/// One file's diff as an HTML fragment (deferred files on big diffs).
async fn diff_file_fragment(web: &WebState, req: &Req, ctx: &RepoCtx, args: &[String], q: &HashMap<String, String>) -> WebResult {
    let [a, b] = args else { return Err(WebError::BadRequest("expected difffile/{base}/{head}".into())) };
    let path = RepoPath::new(q.get("path").cloned().unwrap_or_default()).map_err(|e| WebError::BadRequest(e.to_string()))?;
    if !ctx.config.allows(&req.principal, path.as_bytes(), false) {
        return Err(WebError::NotFound(format!("{path} does not exist")));
    }
    let flux = &web.flux;
    let old = match a.as_str() {
        "-" => None,
        rev => Some(flux.tree_of(ctx.id, &flux.resolve_rev(ctx.id, rev).await?).await?),
    };
    let new = flux.tree_of(ctx.id, &flux.resolve_rev(ctx.id, b).await?).await?;
    let files = diff_files_with(web, old, Some(new), Some(&path), usize::MAX).await?;
    let markup = match files.first() {
        Some(f) => match (&f.diff, &f.note) {
            (Some(d), None) if !d.hunks.is_empty() => diff_table(f, d, None),
            (_, Some(note)) => html! { div.notice.small { (note) } },
            _ => html! { div.notice.small { "No changes." } },
        },
        None => html! { div.notice.small { "No changes." } },
    };
    let mut r = Response::new(axum::body::Body::from(markup.into_string()));
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
    // Both sides are immutable changesets: cache aggressively.
    if a.len() == 64 && b.len() == 64 {
        r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("private, max-age=31536000, immutable"));
    }
    Ok(r)
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
    let mut files = diff_files(web, parent_tree, Some(tree)).await?;
    files.retain(|f| ctx.config.allows(&req.principal, f.path.as_bytes(), false));
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
            (diff_view(ctx, &hex, &files, Some((l.changeset.parents.first().map(|p| p.to_hex()).as_deref().unwrap_or("-"), &hex))))
        }
    };
    let n = nav(web, req, ctx, Tab::Commits, rev).await;
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
        Some(m) => {
            let mut f = diff_files(web, Some(flux.tree_of(ctx.id, &m).await?), Some(flux.tree_of(ctx.id, &h).await?)).await?;
            f.retain(|f| ctx.config.allows(&req.principal, f.path.as_bytes(), false));
            f
        }
        None => vec![],
    };
    let can_write = ctx.can(Role::Write);
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
                (diff_view(ctx, &h.to_hex(), &files, mb.map(|m| (m.to_hex(), h.to_hex())).as_ref().map(|(a, b)| (a.as_str(), b.as_str()))))
            }
        }
    };
    let n = nav(web, req, ctx, Tab::Commits, &head).await;
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
    let n = nav(web, req, ctx, Tab::Branches, &rev).await;
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
    let n = nav(web, req, ctx, Tab::Activity, &rev).await;
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
        for h in r.hits.iter().filter(|h| ctx.config.allows(&req.principal, h.path.as_bytes(), false)) {
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
    let n = nav(web, req, ctx, Tab::Search, &rev).await;
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

async fn files_json(web: &WebState, req: &Req, ctx: &RepoCtx, rev: &str) -> WebResult {
    let cs = web.flux.resolve_rev(ctx.id, rev).await?;
    let root = web.flux.tree_of(ctx.id, &cs).await?;
    let list = web.flux.file_list(root).await?;
    let paths: Vec<String> = list
        .iter()
        .filter(|(p, _, _)| ctx.config.allows(&req.principal, p.as_bytes(), false))
        .map(|(p, _, _)| p.to_string())
        .collect();
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
                @if ctx.can(Role::Write) {
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
    let n = nav(web, req, ctx, Tab::Reviews, &rev).await;
    Ok(page(web, req, &format!("Reviews · {}", ctx.name), Some(n), body))
}

async fn new_review(web: &WebState, req: &Req, ctx: &RepoCtx, q: &HashMap<String, String>) -> WebResult {
    ctx.require(Role::Write)?;
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
    let n = nav(web, req, ctx, Tab::Reviews, &target).await;
    Ok(page(web, req, &format!("New review · {}", ctx.name), Some(n), body))
}

async fn review(web: &WebState, req: &Req, ctx: &RepoCtx, number: i64, files_tab: bool) -> WebResult {
    let flux = &web.flux;
    let d = flux.review_detail(ctx.id, number).await?;
    let r = &d.review;
    let target = String::from_utf8_lossy(&r.target).into_owned();
    let can_write = ctx.can(Role::Write);
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
        let mut f = diff_files_with(web, old, Some(flux.tree_of(ctx.id, &top).await?), None, REVIEW_LINE_BUDGET).await?;
        f.retain(|f| ctx.config.allows(&req.principal, f.path.as_bytes(), false));
        Some(f)
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
                @let comments = Comments {
                    action: format!("{base_url}/comment"),
                    can_write,
                    threads: {
                        let mut m: HashMap<(String, i64), Vec<&flux_store::ReviewEvent>> = HashMap::new();
                        for e in d.events.iter().filter(|e| e.kind == "comment") {
                            if let (Some(p), Some(line)) = (&e.path, e.line) {
                                m.entry((String::from_utf8_lossy(p).into_owned(), line)).or_default().push(e);
                            }
                        }
                        m
                    },
                };
                (diff_view_with(ctx, &r.head.to_hex(), files, Some(&comments), None))
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
                                        div.comment-head {
                                            (avatar(&e.author, "", false)) strong { (e.author) } span.muted { " commented " (time(e.ts_ms)) }
                                            @if let (Some(p), Some(line)) = (&e.path, e.line) {
                                                div.spacer {}
                                                a.comment-loc href=(format!("{base_url}/files")) { (String::from_utf8_lossy(p)) ":" (line.abs()) @if line < 0 { " (old)" } }
                                            }
                                        }
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
    let n = nav(web, req, ctx, Tab::Reviews, &target).await;
    Ok(page(web, req, &format!("{} · Review #{}", r.title, r.number), Some(n), body))
}

#[derive(Deserialize, Default)]
pub struct ReviewForm {
    #[serde(default)]
    method: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    default_branch: String,
    #[serde(default)]
    acl: String,
    #[serde(default)]
    path: String,
    #[serde(default)]
    line: String,
    #[serde(default)]
    base: String,
    #[serde(default)]
    branch: String,
    #[serde(default)]
    content: String,
    #[serde(default)]
    message: String,
    #[serde(default)]
    mode: String,
    #[serde(default)]
    new_branch: String,
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
    let actor = req.principal.name().to_string();
    let flux = &web.flux;
    let segs: Vec<&str> = segs.iter().map(String::as_str).collect();
    match segs.as_slice() {
        ["reviews"] => {
            ctx.require(Role::Write)?;
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
            ctx.require(Role::Write)?;
            let n = parse_num(n)?;
            let method: crate::protect::MergeMethod = f.method.parse()?;
            flux.land_review_as(&req.principal, ctx.id, n, method).await?;
            Ok(Redirect::to(&format!("/{}/-/reviews/{n}", enc(&ctx.name))).into_response())
        }
        ["reviews", n, "comment"] if !f.path.is_empty() => {
            ctx.require(Role::Read)?;
            let n = parse_num(n)?;
            let line: i64 = f.line.trim().parse().map_err(|_| WebError::BadRequest("bad line".into()))?;
            flux.comment_review_at(ctx.id, n, &actor, &f.body, f.path.trim().as_bytes(), line).await?;
            Ok(Redirect::to(&format!("/{}/-/reviews/{n}/files", enc(&ctx.name))).into_response())
        }
        ["reviews", n, action @ ("comment" | "approve" | "request_changes" | "close" | "reopen")] => {
            let n = parse_num(n)?;
            let author = flux.review(ctx.id, n).await?.author == actor;
            ctx.require(crate::rbac::review_action_role(action, author))?;
            flux.update_review(ctx.id, n, action, &actor, &f.body, None).await?;
            Ok(Redirect::to(&format!("/{}/-/reviews/{n}", enc(&ctx.name))).into_response())
        }
        ["edit"] => {
            ctx.require(Role::Write)?;
            flux.check_write_paths(ctx.id, &req.principal, vec![f.path.trim().trim_matches('/').as_bytes().to_vec()]).await?;
            save_edit(&web, &req, &ctx, &actor, f).await
        }
        ["settings"] => {
            ctx.require(Role::Maintain)?;
            let mut acl = Vec::new();
            for line in f.acl.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
                let parts: Vec<&str> = line.split('|').map(str::trim).collect();
                let list = |i: usize| -> Vec<String> {
                    parts
                        .get(i)
                        .map(|s| s.split([',', ' ']).filter(|x| !x.is_empty()).map(str::to_string).collect())
                        .unwrap_or_default()
                };
                acl.push(crate::repo_config::AclRule {
                    path: parts[0].trim_matches('/').to_string(),
                    read: list(1),
                    write: list(2),
                });
            }
            // Keep settings this form does not edit (protection, mirror, ...).
            let cfg = crate::repo_config::RepoConfig {
                description: f.description.trim().to_string(),
                default_branch: Some(f.default_branch.trim().to_string()).filter(|b| !b.is_empty()),
                acl,
                ..(*ctx.config).clone()
            };
            // Path rules are access control: admins only.
            if cfg.acl != ctx.config.acl {
                ctx.require(Role::Admin)?;
            }
            flux.set_repo_config(ctx.id, &cfg).await?;
            flux.audit(&req.principal, "repo.settings", &ctx.name, "").await;
            Ok(Redirect::to(&format!("/{}/-/settings?saved=1", enc(&ctx.name))).into_response())
        }
        _ => Err(WebError::NotFound("no such action".into())),
    }
}

async fn settings(web: &WebState, req: &Req, ctx: &RepoCtx) -> WebResult {
    ctx.require(Role::Maintain)?;
    let saved = query_map(&req.query).contains_key("saved");
    let acl_text: String = ctx
        .config
        .acl
        .iter()
        .map(|r| format!("{} | {} | {}", r.path, r.read.join(", "), r.write.join(", ")))
        .collect::<Vec<_>>()
        .join("\n");
    let default = ctx.default_branch().unwrap_or("main").to_string();
    let body = html! {
        div.container.narrow {
            div.page-head { h1 { "Settings" } p.muted { "Repository details and path permissions." } }
            (settings_tabs(ctx, "general"))
            @if saved { div.notice.ok { (icon("check")) " Saved." } }
            form.card.form method="post" action=(format!("/{}/-/settings", enc(&ctx.name))) {
                label { "Description" input type="text" name="description" value=(ctx.config.description) placeholder="What lives in this repository?"; }
                label { "Default branch"
                    select name="default_branch" {
                        @for (n, _) in ctx.refs.iter() { option value=(n) selected[*n == default] { (n) } }
                    }
                }
                label {
                    "Path permissions"
                    span.hint { "One rule per line: " code { "path/prefix | readers | writers" } ". Principals are user names, " code { "*" } " (anyone with repository access) or " code { "admins" } ". The most specific rule wins; paths without a rule follow the repository's access." }
                    textarea.mono name="acl" rows="6" placeholder="infra/secrets | admins | admins\nservices/payments | * | alice, bob" { (acl_text) }
                }
                div.form-actions { button.btn.primary type="submit" { "Save settings" } }
            }
        }
    };
    let n = nav(web, req, ctx, Tab::Settings, &default).await;
    Ok(page(web, req, &format!("Settings · {}", ctx.name), Some(n), body))
}

fn settings_tabs(ctx: &RepoCtx, active: &str) -> Markup {
    let base = format!("/{}/-/settings", enc(&ctx.name));
    html! {
        nav.filter-tabs.under {
            a.active[active == "general"] href=(base) { (icon("settings")) "General" }
            @if ctx.can(Role::Admin) { a.active[active == "access"] href=(format!("{base}/access")) { (icon("people")) "Access" } }
        }
    }
}

async fn settings_access(web: &WebState, req: &Req, ctx: &RepoCtx) -> WebResult {
    ctx.require(Role::Admin)?;
    let content = super::account::repo_access(web, ctx.id, &ctx.name).await?;
    let q = query_map(&req.query);
    let body = html! {
        div.container.narrow {
            div.page-head { h1 { "Settings" } p.muted { "Who can see and change " (ctx.name) "." } }
            (settings_tabs(ctx, "access"))
            @if let Some(m) = q.get("ok") { div.notice.ok { (icon("check")) (m) } }
            @if let Some(m) = q.get("err") { div.notice.error { (icon("alert")) (m) } }
            (content)
        }
    };
    let default = ctx.default_branch().unwrap_or("main").to_string();
    let n = nav(web, req, ctx, Tab::Settings, &default).await;
    Ok(page(web, req, &format!("Access · {}", ctx.name), Some(n), body))
}

/// POST `/{repo}/-/settings/access`.
pub async fn repo_access_post(web: &WebState, req: &Req, repo: &str, f: &super::account::AccountForm) -> WebResult {
    let ctx = repo_ctx(web, req, repo).await?;
    ctx.require(Role::Admin)?;
    super::account::repo_access_post(web, req, ctx.id, &ctx.name, f).await
}

/// Edit (or create) a file in the browser.
async fn edit_form(
    web: &WebState,
    req: &Req,
    ctx: &RepoCtx,
    rev: &str,
    path: Option<&RepoPath>,
    dir: Option<&RepoPath>,
) -> WebResult {
    ctx.require(Role::Write)?;
    let flux = &web.flux;
    let cs = flux.resolve_rev(ctx.id, rev).await?;
    let content = match path {
        Some(p) => {
            let e = flux.entry_at(ctx.id, &cs, p).await?.ok_or_else(|| WebError::NotFound(format!("{p} not found")))?;
            let cid = e.content_id().ok_or_else(|| WebError::BadRequest("not a regular file".into()))?;
            let data = flux.blobs.get_content_bytes(&cid).await?.unwrap_or_default();
            if is_binary(&data) || data.len() > 1 << 20 {
                return Err(WebError::BadRequest("only text files up to 1 MiB can be edited in the browser".into()));
            }
            String::from_utf8_lossy(&data).into_owned()
        }
        None => String::new(),
    };
    let is_branch = ctx.is_ref(rev);
    let suggested = format!("{}-edit-{}", req.principal.name(), &view::now_ms().to_string()[7..]);
    let prefix = dir.map(|d| format!("{d}/")).unwrap_or_default();
    let body = html! {
        div.container {
            form.card.form.editor method="post" action=(format!("/{}/-/edit", enc(&ctx.name))) {
                input type="hidden" name="base" value=(cs.to_hex());
                input type="hidden" name="branch" value=(rev);
                div.toolbar {
                    span.pill { (icon("branch")) (rev_label(rev)) }
                    @match path {
                        Some(p) => { input type="hidden" name="path" value=(p.to_string()); strong.mono { (p.to_string()) } }
                        None => { input.mono type="text" name="path" value=(prefix) placeholder="path/to/new-file.txt" required autofocus; }
                    }
                }
                textarea.code-editor name="content" rows="28" spellcheck="false" autofocus[path.is_some()] { (content) }
                div.commit-panel {
                    label { "Commit message" input type="text" name="message" required placeholder=(match path { Some(p) => format!("Update {}", p.basename().escape_ascii()), None => "Add a new file".into() }); }
                    div.radio-row {
                        @if is_branch {
                            label.radio { input type="radio" name="mode" value="direct" checked; " Commit directly to " code { (rev) } span.muted.small { " (lands with a server-side rebase if it moved)" } }
                        }
                        label.radio { input type="radio" name="mode" value="review" checked[!is_branch]; " Create a branch and open a review: " input.inline-input type="text" name="new_branch" value=(suggested); }
                    }
                    div.form-actions {
                        a.btn href=(match path { Some(p) => url(&ctx.name, "blob", rev, Some(p)), None => url(&ctx.name, "tree", rev, dir) }) { "Cancel" }
                        button.btn.primary type="submit" { "Commit changes" }
                    }
                }
            }
        }
    };
    let n = nav(web, req, ctx, Tab::Code, rev).await;
    Ok(page(web, req, &format!("Edit · {}", ctx.name), Some(n), body))
}

async fn save_edit(web: &WebState, req: &Req, ctx: &RepoCtx, actor: &str, f: ReviewForm) -> WebResult {
    let flux = &web.flux;
    let base = flux.resolve_rev(ctx.id, &f.base).await?;
    let path = RepoPath::new(f.path.trim().trim_matches('/').as_bytes()).map_err(|e| WebError::BadRequest(e.to_string()))?;
    if f.message.trim().is_empty() {
        return Err(WebError::BadRequest("a commit message is required".into()));
    }
    // Browsers submit CRLF; store what the file had (LF unless it was CRLF).
    let existing = flux.entry_at(ctx.id, &base, &path).await?;
    let mut content = f.content.replace("\r\n", "\n");
    if let Some(cid) = existing.as_ref().and_then(|e| e.content_id())
        && flux.blobs.get_content_bytes(&cid).await?.is_some_and(|d| d.windows(2).any(|w| w == b"\r\n"))
    {
        content = content.replace('\n', "\r\n");
    }
    let kind = existing.as_ref().and_then(|e| e.file_kind()).unwrap_or(FileKind::Regular);
    let (id, meta) = flux.put_content(content.as_bytes()).await?;
    let now = view::now_ms();
    let who = format!("{actor} <{actor}@flux>").into_bytes();
    let mut message = f.message.trim().to_string();
    message.push('\n');
    let cs = flux_core::Changeset {
        parents: vec![base],
        author: who.clone(),
        author_time: flux_core::Timestamp { unix_ms: now, tz_offset_min: 0 },
        committer: Some(who),
        committer_time: Some(flux_core::Timestamp { unix_ms: now, tz_offset_min: 0 }),
        message: message.into_bytes(),
        file_changes: BTreeMap::from([(
            path.clone(),
            flux_core::FileChange::Change { content_id: id, size: meta.size, kind, copy_from: None },
        )]),
        extra: BTreeMap::from([("flux.web_edit".to_string(), b"1".to_vec())]),
    };
    let info = flux.create_changeset(ctx.id, cs).await?;
    let new_id = ChangesetId::from_slice(&info.id).map_err(|e| WebError::Internal(e.to_string()))?;
    if f.mode == "direct" && ctx.is_ref(&f.branch) {
        flux.land_as(&req.principal, ctx.id, new_id, f.branch.as_bytes(), "web edit").await?;
        return Ok(Redirect::to(&url(&ctx.name, "blob", &f.branch, Some(&path))).into_response());
    }
    let branch = f.new_branch.trim();
    if branch.is_empty() {
        return Err(WebError::BadRequest("name the new branch".into()));
    }
    flux.update_ref_as(&req.principal, ctx.id, branch.as_bytes(), flux_store::RefExpect::Absent, Some(new_id), "web edit".into())
        .await?;
    let target = if ctx.is_ref(&f.branch) { f.branch.clone() } else { ctx.default_branch().unwrap_or("main").to_string() };
    let r = flux.create_review(ctx.id, f.message.trim(), "", actor, new_id, target.as_bytes()).await?;
    Ok(Redirect::to(&format!("/{}/-/reviews/{}", enc(&ctx.name), r.number)).into_response())
}
