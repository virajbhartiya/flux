//! Code endpoints: repositories, refs, history, trees, files, search,
//! blame, compare, archives, events, and the agent write path (`changes`).

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::http::{HeaderValue, Method, header};
use axum::response::IntoResponse;
use base64::Engine as _;
use flux_core::{Changeset, ChangesetId, FileChange, FileKind, RepoPath, Timestamp};
use flux_store::{RefExpect, RepoId};
use futures::StreamExt as _;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{ApiResult, Ctx, bad, created, no_content, not_found, reply};
use crate::core::{Flux, FluxResult, Loaded};
use crate::rbac::Role;

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

/// The default branch, or the first of main/master/trunk that exists.
pub async fn default_branch(flux: &Flux, repo: RepoId) -> FluxResult<Option<String>> {
    let cfg = flux.repo_config(repo).await?;
    for b in cfg.default_branch.iter().map(String::as_str).chain(["main", "master", "trunk"]) {
        if flux.meta.get_ref(repo, b.as_bytes()).await?.is_some() {
            return Ok(Some(b.to_string()));
        }
    }
    Ok(cfg.default_branch.clone())
}

pub async fn rev_or_default(c: &Ctx, repo: RepoId, rev: &str) -> ApiResult<ChangesetId> {
    let rev = if rev.is_empty() || rev == "HEAD" {
        default_branch(&c.flux, repo).await?.ok_or_else(|| not_found("the repository is empty"))?
    } else {
        rev.to_string()
    };
    Ok(c.flux.resolve_rev(repo, &rev).await?)
}

fn path_of(p: &str) -> ApiResult<RepoPath> {
    RepoPath::new(p.trim_matches('/').as_bytes().to_vec()).map_err(|e| bad(format!("invalid path {p:?}: {e}")))
}

fn person(sig: &[u8]) -> Value {
    let (name, email) = crate::hooks::split_sig(sig);
    json!({ "name": name, "email": email })
}

pub async fn commit_json(flux: &Flux, repo: RepoId, l: &Loaded, git: bool, files: bool) -> Value {
    let cs = &l.changeset;
    let mut v = json!({
        "id": l.id.to_hex(),
        "parents": cs.parents.iter().map(|p| p.to_hex()).collect::<Vec<_>>(),
        "author": person(&cs.author),
        "author_time": cs.author_time.unix_ms,
        "committer": cs.committer.as_ref().map(|c| person(c)),
        "committer_time": cs.committer_time.map(|t| t.unix_ms),
        "message": String::from_utf8_lossy(&cs.message),
        "generation": l.gen_num,
    });
    if git
        && let Ok(s) = flux.git_commit_shas(repo, &[l.id]).await
        && let Some(s) = s.first()
    {
        v["git_sha"] = json!(hex::encode(s));
    }
    if files {
        v["files"] = json!(
            cs.file_changes
                .iter()
                .map(|(p, ch)| json!({
                    "path": p.to_string(),
                    "change": match ch { FileChange::Deletion => "deleted", FileChange::Retain => "retained", FileChange::Change { copy_from: Some(_), .. } => "copied", FileChange::Change { .. } => "modified" },
                    "size": match ch { FileChange::Change { size, .. } => Some(*size), _ => None },
                }))
                .collect::<Vec<_>>()
        );
    }
    v
}

// ───────────── repositories ─────────────

pub async fn list_repos(c: &Ctx) -> ApiResult {
    let repos = c.flux.readable_repos(&c.who).await?;
    let mut out = Vec::with_capacity(repos.len());
    for r in repos {
        out.push(repo_summary(c, r.id, &r.name).await?);
    }
    reply(out)
}

async fn repo_summary(c: &Ctx, repo: RepoId, name: &str) -> ApiResult<Value> {
    let cfg = c.flux.repo_config(repo).await?;
    let access = c.flux.repo_access(repo).await?;
    let base = c.base_url();
    Ok(json!({
        "id": repo.0,
        "full_name": name,
        "description": cfg.description,
        "default_branch": default_branch(&c.flux, repo).await?,
        "visibility": access.visibility,
        "archived": cfg.archived,
        "secret_scanning": !cfg.secret_scanning_disabled,
        "html_url": format!("{base}/{name}"),
        "clone_url": format!("{base}/{name}.git"),
        "ssh_url": c.flux.settings.ssh_host.as_ref().map(|h| format!("ssh://git@{h}/{name}.git")),
    }))
}

#[derive(Deserialize)]
struct NewRepo {
    name: String,
    #[serde(default)]
    visibility: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    default_branch: Option<String>,
    #[serde(default = "yes")]
    seed_labels: bool,
}

fn yes() -> bool {
    true
}

pub async fn create_repo(c: &Ctx) -> ApiResult {
    let r: NewRepo = c.parse()?;
    let id = c.flux.create_repo_as(&c.who, &r.name, r.visibility.as_deref()).await?;
    if r.description.is_some() || r.default_branch.is_some() {
        let mut cfg = (*c.flux.repo_config(id).await?).clone();
        cfg.description = r.description.unwrap_or_default();
        cfg.default_branch = r.default_branch;
        c.flux.set_repo_config(id, &cfg).await?;
    }
    if r.seed_labels {
        c.flux.seed_labels(id).await?;
    }
    created(repo_summary(c, id, &r.name).await?)
}

pub async fn repo_info(c: &Ctx, repo: RepoId) -> ApiResult {
    c.flux.authorize(&c.who, repo, Role::Read).await?;
    let name = c.flux.repo_name(repo).await;
    let mut v = repo_summary(c, repo, &name).await?;
    let cfg = c.flux.repo_config(repo).await?;
    let (open_issues, _) = c.flux.meta.issue_counts(repo).await?;
    v["open_issues"] = json!(open_issues);
    v["open_reviews"] = json!(c.flux.meta.review_count(repo, "open").await?);
    v["changesets"] = json!(c.flux.meta.changeset_count(repo).await?);
    v["topics"] = json!(cfg.topics);
    v["homepage"] = json!(cfg.homepage);
    v["fork_of"] = match c.flux.meta.fork_of(repo).await? {
        Some(p) => json!(c.flux.repo_name(p).await),
        None => Value::Null,
    };
    v["permission"] = json!(c.flux.repo_role(&c.who, repo).await?.as_str());
    reply(v)
}

#[derive(Deserialize)]
struct RepoPatch {
    name: Option<String>,
    description: Option<String>,
    default_branch: Option<String>,
    visibility: Option<String>,
    homepage: Option<String>,
    topics: Option<Vec<String>>,
    archived: Option<bool>,
    has_issues: Option<bool>,
    /// Push protection (credential scanning of new content).
    secret_scanning: Option<bool>,
}

pub async fn update_repo(c: &Ctx, repo: RepoId) -> ApiResult {
    let p: RepoPatch = c.parse()?;
    let role = c.flux.authorize(&c.who, repo, Role::Maintain).await?;
    let mut cfg = (*c.flux.repo_config(repo).await?).clone();
    if let Some(d) = p.description {
        cfg.description = d;
    }
    if let Some(b) = p.default_branch {
        cfg.default_branch = Some(b).filter(|b| !b.is_empty());
    }
    if let Some(h) = p.homepage {
        cfg.homepage = h;
    }
    if let Some(t) = p.topics {
        cfg.topics = t.into_iter().take(20).collect();
    }
    if let Some(i) = p.has_issues {
        cfg.issues_disabled = !i;
    }
    if let Some(on) = p.secret_scanning {
        if role < Role::Admin {
            return Err(super::ApiError(axum::http::StatusCode::FORBIDDEN, "changing push protection needs admin access".into()));
        }
        cfg.secret_scanning_disabled = !on;
    }
    if let Some(a) = p.archived {
        if role < Role::Admin {
            return Err(super::ApiError(axum::http::StatusCode::FORBIDDEN, "archiving needs admin access".into()));
        }
        cfg.archived = a;
    }
    c.flux.set_repo_config(repo, &cfg).await?;
    let mut name = c.flux.repo_name(repo).await;
    if let Some(v) = p.visibility {
        c.flux.set_visibility(&c.who, repo, &name, &v).await?;
    }
    if let Some(n) = p.name.filter(|n| *n != name) {
        c.flux.rename_repo_as(&c.who, repo, &n).await?;
        name = n;
    }
    c.flux.audit(&c.who, "repo.settings", &name, "api").await;
    reply(repo_summary(c, repo, &name).await?)
}

// ───────────── refs ─────────────

pub async fn list_refs(c: &Ctx, repo: RepoId, kind: &str) -> ApiResult {
    c.flux.authorize(&c.who, repo, Role::Read).await?;
    let prefix = c.q("prefix").unwrap_or(if kind == "tags" { "tags/" } else { "" });
    let refs = c.flux.meta.list_refs(repo, prefix.as_bytes(), 100_000).await?;
    let hidden = |n: &[u8]| n.starts_with(b"pull/") || n.starts_with(b"review/");
    let out: Vec<Value> = refs
        .iter()
        .filter(|(n, _)| match kind {
            "branches" => !n.starts_with(b"tags/") && !hidden(n),
            "tags" => n.starts_with(b"tags/"),
            _ => c.flag("all") || !hidden(n),
        })
        .map(|(n, v)| {
            let name = String::from_utf8_lossy(n);
            json!({ "name": if kind == "tags" { name.trim_start_matches("tags/").to_string() } else { name.to_string() }, "target": v.target.to_hex(), "version": v.version })
        })
        .collect();
    reply(out)
}

#[derive(Deserialize)]
struct RefPut {
    target: String,
    /// Expected current target (hex or rev); omitted = create only if absent
    /// unless `force`.
    #[serde(default)]
    expect: Option<String>,
    #[serde(default)]
    force: bool,
}

pub async fn ref_op(c: &Ctx, repo: RepoId, name: &str) -> ApiResult {
    match c.method {
        Method::GET => {
            c.flux.authorize(&c.who, repo, Role::Read).await?;
            let v = c.flux.meta.get_ref(repo, name.as_bytes()).await?.ok_or_else(|| not_found(format!("no ref {name}")))?;
            reply(json!({ "name": name, "target": v.target.to_hex(), "version": v.version }))
        }
        Method::PUT | Method::POST | Method::PATCH => {
            let r: RefPut = c.parse()?;
            let target = c.flux.resolve_rev(repo, &r.target).await?;
            let expect = match (&r.expect, r.force) {
                (Some(e), _) => RefExpect::Target(c.flux.resolve_rev(repo, e).await?),
                (None, true) => RefExpect::Any,
                (None, false) => match c.flux.meta.get_ref(repo, name.as_bytes()).await? {
                    // Plain updates must fast-forward; protection enforces the rest.
                    Some(cur) if c.flux.is_ancestor(repo, &cur.target, &target).await? => RefExpect::Version(cur.version),
                    Some(_) => {
                        return Err(super::ApiError(
                            axum::http::StatusCode::CONFLICT,
                            "not a fast-forward; pass expect or force".into(),
                        ));
                    }
                    None => RefExpect::Absent,
                },
            };
            let (outcome, current) =
                c.flux.update_ref_as(&c.who, repo, name.as_bytes(), expect, Some(target), "api".into()).await?;
            match outcome {
                flux_store::CasOutcome::Mismatch { .. } => Err(super::ApiError(
                    axum::http::StatusCode::CONFLICT,
                    format!("ref moved (now {})", current.map(|c| c.target.to_hex()).unwrap_or_else(|| "absent".into())),
                )),
                _ => reply(json!({ "name": name, "target": target.to_hex() })),
            }
        }
        Method::DELETE => {
            let cur = c.flux.meta.get_ref(repo, name.as_bytes()).await?.ok_or_else(|| not_found(format!("no ref {name}")))?;
            c.flux.update_ref_as(&c.who, repo, name.as_bytes(), RefExpect::Version(cur.version), None, "api".into()).await?;
            no_content()
        }
        _ => Err(super::method_not_allowed()),
    }
}

// ───────────── history ─────────────

pub async fn log(c: &Ctx, repo: RepoId) -> ApiResult {
    c.flux.authorize(&c.who, repo, Role::Read).await?;
    let head = rev_or_default(c, repo, c.q("rev").or(c.q("sha")).unwrap_or("")).await?;
    let (limit, offset) = c.page(30, 1000)?;
    let path = c.q("path").map(path_of).transpose()?;
    if let Some(p) = &path {
        c.flux.check_read_path(repo, &c.who, p.as_bytes()).await?;
    }
    let hide: Vec<ChangesetId> = match c.q("since") {
        Some(s) => vec![c.flux.resolve_rev(repo, s).await?],
        None => vec![],
    };
    let page =
        c.flux.log_page(repo, &head, &hide, path.as_ref(), c.flag("first_parent"), offset as usize, limit as usize).await?;
    let git = c.flag("git");
    let mut out = Vec::with_capacity(page.len());
    for l in &page {
        out.push(commit_json(&c.flux, repo, l, git, c.flag("files")).await);
    }
    reply(out)
}

pub async fn commit(c: &Ctx, repo: RepoId, rev: &str) -> ApiResult {
    c.flux.authorize(&c.who, repo, Role::Read).await?;
    let id = c.flux.resolve_rev(repo, rev).await?;
    let l = c.flux.load_one_arc(repo, &id).await?;
    let mut v = commit_json(&c.flux, repo, &l, true, true).await;
    if c.flag("patch") || c.flag("diff") {
        let parent = l.changeset.parents.first().copied();
        v["patch"] = json!(unified_diff(&c.flux, repo, &c.who, parent, Some(id), None).await?);
    }
    v["status"] = json!(c.flux.combined_status(repo, &id).await?.state);
    reply(v)
}

pub async fn combined_status(c: &Ctx, repo: RepoId, rev: &str) -> ApiResult {
    c.flux.authorize(&c.who, repo, Role::Read).await?;
    let id = c.flux.resolve_rev(repo, rev).await?;
    let wait = c.num("wait", 0)?.clamp(0, 600) as u64;
    let combined = if wait > 0 {
        let min = c.num("min", 1)?.max(0) as usize;
        c.flux.wait_for_checks(repo, &id, min, std::time::Duration::from_secs(wait)).await?
    } else {
        c.flux.combined_status(repo, &id).await?
    };
    reply(
        json!({ "changeset": id.to_hex(), "state": combined.state, "total_count": combined.total, "statuses": combined.statuses }),
    )
}

pub async fn list_statuses(c: &Ctx, repo: RepoId, rev: &str) -> ApiResult {
    c.flux.authorize(&c.who, repo, Role::Read).await?;
    let id = c.flux.resolve_rev(repo, rev).await?;
    reply(c.flux.combined_status(repo, &id).await?.statuses)
}

#[derive(Deserialize)]
struct StatusIn {
    state: String,
    #[serde(default)]
    context: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    target_url: String,
}

pub async fn post_status(c: &Ctx, repo: RepoId, rev: &str) -> ApiResult {
    let s: StatusIn = c.parse()?;
    let id = c.flux.resolve_rev(repo, rev).await?;
    c.flux.post_status(&c.who, repo, id, &s.state, &s.context, &s.description, &s.target_url).await?;
    created(
        json!({ "changeset": id.to_hex(), "state": s.state, "context": if s.context.is_empty() { "default" } else { &s.context } }),
    )
}

// ───────────── trees and files ─────────────

pub async fn tree(c: &Ctx, repo: RepoId, rev: &str, path: &str) -> ApiResult {
    c.flux.authorize(&c.who, repo, Role::Read).await?;
    let id = rev_or_default(c, repo, rev).await?;
    let root = c.flux.root_tree(repo, &id).await?;
    let acl = c.flux.repo_config(repo).await?;
    let base = path.trim_matches('/');
    let dir = if base.is_empty() {
        root
    } else {
        let p = path_of(base)?;
        if !acl.dir_visible(&c.who, p.as_bytes()) {
            return Err(not_found("not found"));
        }
        match flux_derive::lookup_path(&c.flux.blobs, root, &p).await? {
            Some(e) if e.is_dir() => e.tree_id().expect("directory"),
            Some(_) => return Err(bad(format!("{base} is a file"))),
            None => return Err(not_found(format!("{base} not found"))),
        }
    };
    if c.flag("recursive") {
        let limit = c.num("limit", 100_000)?.clamp(1, 1_000_000) as usize;
        let files = flux_derive::walk_tree(&c.flux.blobs, dir, &|_, _| true).await?;
        let total = files.len();
        let entries: Vec<Value> = files
            .iter()
            .map(|f| (if base.is_empty() { f.path.to_string() } else { format!("{base}/{}", f.path) }, f))
            .filter(|(p, _)| acl.allows(&c.who, p.as_bytes(), false))
            .take(limit)
            .map(|(p, f)| json!({ "path": p, "type": kind_name(f.kind), "size": f.size }))
            .collect();
        return reply(json!({ "changeset": id.to_hex(), "path": base, "truncated": total > limit, "entries": entries }));
    }
    let t = c.flux.blobs.get_tree(&dir).await?.ok_or_else(|| not_found("tree missing"))?;
    let entries: Vec<Value> = t
        .entries()
        .iter()
        .map(|e| {
            let name = String::from_utf8_lossy(e.name.as_bytes()).into_owned();
            let full = if base.is_empty() { name.clone() } else { format!("{base}/{name}") };
            (full, name, e)
        })
        .filter(|(full, _, _)| acl.dir_visible(&c.who, full.as_bytes()))
        .map(|(full, name, e)| {
            json!({
                "name": name,
                "path": full,
                "type": match e.file_kind() { Some(k) => kind_name(k), None => "dir" },
                "size": e.size,
                "files": if e.is_dir() { Some(e.descendant_files) } else { None },
            })
        })
        .collect();
    reply(json!({ "changeset": id.to_hex(), "path": base, "entries": entries }))
}

fn kind_name(k: FileKind) -> &'static str {
    match k {
        FileKind::Symlink => "symlink",
        FileKind::GitLink(_) => "submodule",
        k if k.is_executable() => "executable",
        _ => "file",
    }
}

/// A file's content as JSON: UTF-8 text inline, binary as base64.
fn file_value(path: &str, data: &[u8], max: usize, lines: Option<(usize, usize)>) -> Value {
    let binary = crate::web_util::is_binary(data);
    if binary {
        let truncated = data.len() > max;
        return json!({ "path": path, "size": data.len(), "binary": true, "encoding": "base64",
            "content": B64.encode(&data[..data.len().min(max)]), "truncated": truncated });
    }
    let text = String::from_utf8_lossy(data);
    let total_lines = text.lines().count();
    let (content, truncated) = match lines {
        Some((start, end)) => {
            let start = start.max(1);
            let selected: Vec<&str> = text.lines().skip(start - 1).take(end.saturating_sub(start) + 1).collect();
            (selected.join("\n"), false)
        }
        None if data.len() > max => {
            let mut cut = max;
            while cut > 0 && !text.is_char_boundary(cut) {
                cut -= 1;
            }
            (text[..cut].to_string(), true)
        }
        None => (text.into_owned(), false),
    };
    json!({ "path": path, "size": data.len(), "binary": false, "encoding": "utf-8", "lines": total_lines, "content": content, "truncated": truncated })
}

async fn read_at(flux: &Flux, repo: RepoId, cs: &ChangesetId, path: &RepoPath) -> FluxResult<Option<(FileKind, bytes::Bytes)>> {
    let Some(e) = flux.entry_at(repo, cs, path).await? else { return Ok(None) };
    let (Some(k), Some(cid)) = (e.file_kind(), e.content_id()) else { return Ok(None) };
    Ok(flux.blobs.get_content_bytes(&cid).await?.map(|d| (k, d)))
}

pub async fn contents(c: &Ctx, repo: RepoId, path: &str) -> ApiResult {
    c.flux.authorize(&c.who, repo, Role::Read).await?;
    let id = rev_or_default(c, repo, c.q("ref").or(c.q("rev")).unwrap_or("")).await?;
    if path.trim_matches('/').is_empty() {
        return tree(c, repo, &id.to_hex(), "").await;
    }
    let p = path_of(path)?;
    c.flux.check_read_path(repo, &c.who, p.as_bytes()).await?;
    let e = c.flux.entry_at(repo, &id, &p).await?.ok_or_else(|| not_found(format!("{path} not found")))?;
    if e.is_dir() {
        return tree(c, repo, &id.to_hex(), path).await;
    }
    let (_, data) = read_at(&c.flux, repo, &id, &p).await?.ok_or_else(|| not_found(format!("{path} has no content")))?;
    let mut v = file_value(&p.to_string(), &data, c.num("max_bytes", 4 << 20)? as usize, line_range(c)?);
    v["changeset"] = json!(id.to_hex());
    reply(v)
}

fn line_range(c: &Ctx) -> ApiResult<Option<(usize, usize)>> {
    Ok(match (c.q("start_line"), c.q("end_line")) {
        (None, None) => None,
        (s, e) => Some((
            s.map(|s| s.parse()).transpose().map_err(|_| bad("start_line"))?.unwrap_or(1),
            e.map(|e| e.parse()).transpose().map_err(|_| bad("end_line"))?.unwrap_or(usize::MAX),
        )),
    })
}

pub async fn raw(c: &Ctx, repo: RepoId, rev: &str, path: &str) -> ApiResult {
    c.flux.authorize(&c.who, repo, Role::Read).await?;
    let id = c.flux.resolve_rev(repo, rev).await?;
    let p = path_of(path)?;
    c.flux.check_read_path(repo, &c.who, p.as_bytes()).await?;
    let (_, data) = read_at(&c.flux, repo, &id, &p).await?.ok_or_else(|| not_found(format!("{path} not found")))?;
    let mut r = data.into_response();
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("private, max-age=60"));
    Ok(r)
}

#[derive(Deserialize)]
struct ReadReq {
    #[serde(default)]
    rev: String,
    #[serde(default)]
    paths: Vec<String>,
    #[serde(default)]
    files: Vec<ReadSpec>,
    #[serde(default)]
    max_bytes: Option<usize>,
}

#[derive(Deserialize)]
struct ReadSpec {
    path: String,
    #[serde(default)]
    start_line: Option<usize>,
    #[serde(default)]
    end_line: Option<usize>,
}

/// Read many files at one revision in one call (agents read context in
/// bulk; this is one round trip instead of dozens).
pub async fn read_files(c: &Ctx, repo: RepoId) -> ApiResult {
    c.flux.authorize(&c.who, repo, Role::Read).await?;
    let r: ReadReq = c.parse()?;
    let id = rev_or_default(c, repo, &r.rev).await?;
    let mut specs: Vec<ReadSpec> = r.paths.into_iter().map(|path| ReadSpec { path, start_line: None, end_line: None }).collect();
    specs.extend(r.files);
    if specs.len() > 1000 {
        return Err(bad("at most 1000 files per call"));
    }
    let max = r.max_bytes.unwrap_or(1 << 20).min(16 << 20);
    let acl = c.flux.repo_config(repo).await?;
    // Futures are built eagerly with owned state (no borrowing closures),
    // then run 32 at a time in order.
    let futs: Vec<_> = specs
        .into_iter()
        .map(|s| {
            let flux = c.flux.clone();
            let acl = acl.clone();
            let who = c.who.clone();
            async move {
                let p = match path_of(&s.path) {
                    Ok(p) => p,
                    Err(e) => return json!({ "path": s.path, "error": e.1 }),
                };
                if !acl.allows(&who, p.as_bytes(), false) {
                    return json!({ "path": s.path, "error": "not found" });
                }
                match read_at(&flux, repo, &id, &p).await {
                    Ok(Some((_, data))) => {
                        let range = match (s.start_line, s.end_line) {
                            (None, None) => None,
                            (a, b) => Some((a.unwrap_or(1), b.unwrap_or(usize::MAX))),
                        };
                        file_value(&s.path, &data, max, range)
                    }
                    Ok(None) => json!({ "path": s.path, "error": "not found" }),
                    Err(e) => json!({ "path": s.path, "error": e.to_string() }),
                }
            }
        })
        .collect();
    let results: Vec<Value> = futures::stream::iter(futs).buffered(32).collect().await;
    reply(json!({ "changeset": id.to_hex(), "files": results }))
}

// ───────────── agent writes ─────────────

#[derive(Deserialize)]
struct ChangeReq {
    #[serde(default)]
    base: Option<String>,
    #[serde(default)]
    branch: Option<String>,
    message: String,
    #[serde(default)]
    author: Option<String>,
    files: Vec<FileOp>,
    #[serde(default)]
    review: Option<ReviewReq>,
    /// Land directly onto this branch (rebasing if it moved).
    #[serde(default)]
    land: Option<String>,
    /// Allow moving an existing branch that does not point at `base`.
    #[serde(default)]
    force: bool,
}

#[derive(Deserialize)]
struct FileOp {
    path: String,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    content_base64: Option<String>,
    #[serde(default)]
    delete: bool,
    #[serde(default)]
    edits: Option<Vec<Edit>>,
    #[serde(default)]
    executable: Option<bool>,
    #[serde(default)]
    move_from: Option<String>,
    #[serde(default)]
    symlink: Option<String>,
}

#[derive(Deserialize)]
struct Edit {
    old: String,
    new: String,
    /// Replace every occurrence (default: exactly one must exist).
    #[serde(default)]
    all: bool,
}

#[derive(Deserialize)]
struct ReviewReq {
    title: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    target: Option<String>,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    reviewers: Vec<String>,
    #[serde(default)]
    labels: Vec<String>,
}

/// Apply search/replace edits to text.
fn apply_edits(path: &str, text: &str, edits: &[Edit]) -> ApiResult<String> {
    let mut out = text.to_string();
    for (i, e) in edits.iter().enumerate() {
        if e.old.is_empty() {
            return Err(bad(format!("{path}: edit {i} has an empty `old`")));
        }
        let n = out.matches(e.old.as_str()).count();
        match (n, e.all) {
            (0, _) => return Err(bad(format!("{path}: edit {i}: `old` text not found"))),
            (1, _) | (_, true) => out = out.replace(e.old.as_str(), &e.new),
            (n, false) => {
                return Err(bad(format!("{path}: edit {i}: `old` text matches {n} times; add context or set all=true")));
            }
        }
    }
    Ok(out)
}

/// Create a changeset from file operations, optionally moving a branch,
/// opening a review or landing it — one call, no clone, no working copy.
pub async fn changes(c: &Ctx, repo: RepoId) -> ApiResult {
    let r: ChangeReq = c.parse()?;
    let flux = &c.flux;
    flux.authorize(&c.who, repo, Role::Write).await?;
    flux.check_writable(repo).await?;
    if r.files.is_empty() {
        return Err(bad("no files to change"));
    }
    if r.files.len() > 10_000 {
        return Err(bad("at most 10000 files per change"));
    }
    if r.message.trim().is_empty() {
        return Err(bad("a message is required"));
    }
    let branch_now = match &r.branch {
        Some(b) => flux.meta.get_ref(repo, b.as_bytes()).await?,
        None => None,
    };
    let base: Option<ChangesetId> = match (&r.base, &branch_now) {
        (Some(b), _) => Some(flux.resolve_rev(repo, b).await?),
        (None, Some(cur)) => Some(cur.target),
        (None, None) => match default_branch(flux, repo).await? {
            Some(d) => flux.meta.get_ref(repo, d.as_bytes()).await?.map(|v| v.target),
            None => None,
        },
    };
    if let (Some(cur), Some(b)) = (&branch_now, base)
        && cur.target != b
        && !r.force
    {
        return Err(super::ApiError(
            axum::http::StatusCode::CONFLICT,
            format!(
                "branch {} is at {}, not at base {}; omit base or pass force",
                r.branch.as_deref().unwrap_or(""),
                cur.target.to_hex(),
                b.to_hex()
            ),
        ));
    }
    let mut fc: BTreeMap<RepoPath, FileChange> = BTreeMap::new();
    let mut uploads: Vec<(RepoPath, FileKind, bytes::Bytes)> = Vec::new();
    for op in &r.files {
        let p = path_of(&op.path)?;
        let existing = match base {
            Some(b) => flux.entry_at(repo, &b, &p).await?,
            None => None,
        };
        if existing.as_ref().is_some_and(|e| e.is_dir()) {
            return Err(bad(format!("{} is a directory", op.path)));
        }
        let old_kind = existing.as_ref().and_then(|e| e.file_kind());
        let kind = |default: FileKind| match op.executable {
            Some(true) => FileKind::Executable,
            Some(false) => FileKind::Regular,
            None => old_kind.filter(|k| k.is_plain()).unwrap_or(default),
        };
        if let Some(from) = &op.move_from {
            let fp = path_of(from)?;
            let b = base.ok_or_else(|| bad("move_from needs a base"))?;
            let e = flux.entry_at(repo, &b, &fp).await?.ok_or_else(|| bad(format!("{from} does not exist")))?;
            let (Some(k), Some(cid)) = (e.file_kind(), e.content_id()) else { return Err(bad(format!("{from} is not a file"))) };
            fc.insert(fp.clone(), FileChange::Deletion);
            if op.content.is_none() && op.content_base64.is_none() && op.edits.is_none() {
                fc.insert(p, FileChange::Change { content_id: cid, size: e.size, kind: k, copy_from: Some((fp, 0)) });
                continue;
            }
        }
        if op.delete {
            if existing.is_none() {
                return Err(bad(format!("cannot delete {}: it does not exist", op.path)));
            }
            fc.insert(p, FileChange::Deletion);
            continue;
        }
        let data: Vec<u8> = if let Some(t) = &op.symlink {
            uploads.push((p, FileKind::Symlink, bytes::Bytes::from(t.clone().into_bytes())));
            continue;
        } else if let Some(t) = &op.content {
            t.clone().into_bytes()
        } else if let Some(b) = &op.content_base64 {
            B64.decode(b.trim()).map_err(|e| bad(format!("{}: bad base64: {e}", op.path)))?
        } else if let Some(edits) = &op.edits {
            let src = match &op.move_from {
                Some(f) => path_of(f)?,
                None => p.clone(),
            };
            let b = base.ok_or_else(|| bad("edits need a base"))?;
            let (_, old) =
                read_at(flux, repo, &b, &src).await?.ok_or_else(|| bad(format!("cannot edit {}: it does not exist", op.path)))?;
            let text = std::str::from_utf8(&old).map_err(|_| bad(format!("cannot edit {}: not UTF-8 text", op.path)))?;
            apply_edits(&op.path, text, edits)?.into_bytes()
        } else if op.executable.is_some() {
            // Mode change only.
            let b = base.ok_or_else(|| bad("mode changes need a base"))?;
            read_at(flux, repo, &b, &p).await?.ok_or_else(|| bad(format!("{} does not exist", op.path)))?.1.to_vec()
        } else {
            return Err(bad(format!("{}: give content, content_base64, edits, delete, symlink or move_from", op.path)));
        };
        uploads.push((p, kind(FileKind::Regular), bytes::Bytes::from(data)));
    }
    let scanned: Vec<(String, bytes::Bytes)> = uploads.iter().map(|(p, _, d)| (p.to_string(), d.clone())).collect();
    flux.scan_new_contents(repo, &scanned).await?;
    let stored = flux.put_contents(uploads.iter().map(|(_, _, d)| (d.clone(), None)).collect()).await?;
    for ((p, k, _), (cid, meta)) in uploads.into_iter().zip(stored) {
        fc.insert(p, FileChange::Change { content_id: cid, size: meta.size, kind: k, copy_from: None });
    }
    flux.check_write_paths(repo, &c.who, fc.keys().map(|p| p.as_bytes().to_vec()).collect()).await?;
    let user = c.user()?;
    let email =
        flux.meta.get_account_by_id(user.id).await?.and_then(|a| a.email).unwrap_or_else(|| format!("{}@flux", user.name));
    let me = format!("{} <{email}>", user.name).into_bytes();
    let now = Timestamp { unix_ms: crate::auth::now_ms(), tz_offset_min: 0 };
    let cs = Changeset {
        parents: base.into_iter().collect(),
        author: r.author.map(String::into_bytes).unwrap_or_else(|| me.clone()),
        author_time: now,
        committer: Some(me),
        committer_time: Some(now),
        message: if r.message.ends_with('\n') { r.message.into_bytes() } else { format!("{}\n", r.message).into_bytes() },
        file_changes: fc,
        extra: BTreeMap::new(),
    };
    let n_files = cs.file_changes.len();
    let info = flux.create_changeset(repo, cs).await?;
    let id = ChangesetId::from_slice(&info.id).map_err(|e| bad(e.to_string()))?;
    let mut out = json!({ "changeset": id.to_hex(), "parent": base.map(|b| b.to_hex()), "files_changed": n_files });
    if let Some(b) = &r.branch {
        let expect = match &branch_now {
            Some(cur) => RefExpect::Version(cur.version),
            None => RefExpect::Absent,
        };
        let (outcome, _) = flux.update_ref_as(&c.who, repo, b.as_bytes(), expect, Some(id), "api change".into()).await?;
        if matches!(outcome, flux_store::CasOutcome::Mismatch { .. }) {
            return Err(super::ApiError(axum::http::StatusCode::CONFLICT, format!("branch {b} moved; retry with the new base")));
        }
        out["branch"] = json!(b);
    }
    if let Some(target) = &r.land {
        let landed = flux.land_as(&c.who, repo, id, target.as_bytes(), "api change").await?;
        out["landed"] = json!({ "branch": target, "head": landed.head.to_hex(), "fast_forward": landed.fast_forward });
    }
    if let Some(rv) = r.review {
        let target = match rv.target {
            Some(t) => t,
            None => default_branch(flux, repo).await?.unwrap_or_else(|| "main".into()),
        };
        let review = flux.create_review(repo, &rv.title, &rv.body, c.who.name(), id, target.as_bytes()).await?;
        if rv.draft {
            flux.meta.set_review_draft(repo, review.number, true).await?;
        }
        let role = flux.repo_role(&c.who, repo).await?;
        if !rv.labels.is_empty() {
            flux.apply_labels(&c.who, repo, review.number, &rv.labels, role).await?;
        }
        if !rv.reviewers.is_empty() {
            flux.set_people(&c.who, repo, review.number, "reviewer", &rv.reviewers).await?;
        }
        let name = flux.repo_name(repo).await;
        out["review"] = json!({ "number": review.number, "url": format!("{}/{name}/-/reviews/{}", c.base_url(), review.number) });
    }
    created(out)
}

// ───────────── compare, search, blame ─────────────

/// A unified diff between two changesets (either may be absent).
pub async fn unified_diff(
    flux: &Flux,
    repo: RepoId,
    who: &crate::auth::Principal,
    old: Option<ChangesetId>,
    new: Option<ChangesetId>,
    path: Option<&RepoPath>,
) -> FluxResult<String> {
    let a = match old {
        Some(o) => Some(flux.root_tree(repo, &o).await?),
        None => None,
    };
    let b = match new {
        Some(n) => Some(flux.root_tree(repo, &n).await?),
        None => None,
    };
    let filter = |p: &RepoPath, _: bool| {
        path.is_none_or(|f| p.as_bytes().starts_with(f.as_bytes()) || f.as_bytes().starts_with(p.as_bytes()))
    };
    let diff = flux_derive::diff_trees(&flux.blobs, a, b, &filter).await?;
    let acl = flux.repo_config(repo).await?;
    let mut out = String::new();
    for d in diff {
        if !acl.allows(who, d.path.as_bytes(), false) {
            continue;
        }
        let read = |f: &Option<flux_derive::TreeFile>| {
            let cid = f.as_ref().and_then(|f| f.content_id);
            async move {
                match cid {
                    Some(c) => flux.blobs.get_content_bytes(&c).await.map(|d| d.unwrap_or_default()),
                    None => Ok(bytes::Bytes::new()),
                }
            }
        };
        let (od, nd) = (read(&d.old).await?, read(&d.new).await?);
        let p = d.path.to_string();
        let (from, to) = (
            if d.old.is_some() { format!("a/{p}") } else { "/dev/null".into() },
            if d.new.is_some() { format!("b/{p}") } else { "/dev/null".into() },
        );
        out.push_str(&format!("diff --flux a/{p} b/{p}\n"));
        if crate::web_util::is_binary(&od) || crate::web_util::is_binary(&nd) {
            out.push_str(&format!("Binary files {from} and {to} differ\n"));
            continue;
        }
        let (os, ns) = (String::from_utf8_lossy(&od), String::from_utf8_lossy(&nd));
        let text = similar::TextDiff::from_lines(os.as_ref(), ns.as_ref());
        out.push_str(&text.unified_diff().context_radius(3).header(&from, &to).to_string());
    }
    Ok(out)
}

pub async fn compare(c: &Ctx, repo: RepoId, spec: &str) -> ApiResult {
    c.flux.authorize(&c.who, repo, Role::Read).await?;
    let (base, head) = spec.split_once("...").or_else(|| spec.split_once("..")).ok_or_else(|| bad("use base...head"))?;
    let (b, h) = (c.flux.resolve_rev(repo, base).await?, c.flux.resolve_rev(repo, head).await?);
    // Three-dot: diff from the merge base, like GitHub.
    let from = if spec.contains("...") { c.flux.merge_base(repo, &b, &h).await?.unwrap_or(b) } else { b };
    let path = c.q("path").map(path_of).transpose()?;
    let patch = unified_diff(&c.flux, repo, &c.who, Some(from), Some(h), path.as_ref()).await?;
    if c.q("format") == Some("patch")
        || c.headers.get(header::ACCEPT).is_some_and(|a| a.to_str().unwrap_or("").contains("text/x-diff"))
    {
        let mut r = patch.into_response();
        r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/x-diff; charset=utf-8"));
        return Ok(r);
    }
    let (ahead, behind) = c.flux.ahead_behind(repo, &h, &b).await?;
    let commits = c.flux.log_page(repo, &h, &[b], None, false, 0, 250).await?;
    let mut cs = Vec::new();
    for l in commits.iter().rev() {
        cs.push(commit_json(&c.flux, repo, l, false, false).await);
    }
    reply(
        json!({ "base": b.to_hex(), "head": h.to_hex(), "merge_base": from.to_hex(), "ahead_by": ahead, "behind_by": behind, "commits": cs, "patch": patch }),
    )
}

pub async fn search(c: &Ctx, repo: RepoId) -> ApiResult {
    c.flux.authorize(&c.who, repo, Role::Read).await?;
    let q = c.q("q").ok_or_else(|| bad("q is required"))?;
    let id = rev_or_default(c, repo, c.q("rev").unwrap_or("")).await?;
    let root = c.flux.root_tree(repo, &id).await?;
    let prefix = c.q("path").map(path_of).transpose()?;
    let limit = c.num("limit", 200)?.clamp(1, 10_000) as usize;
    let res = c.flux.search(repo, root, q, c.flag("regex"), !c.flag("case_sensitive"), prefix.as_ref(), limit).await?;
    let acl = c.flux.repo_config(repo).await?;
    let hits: Vec<Value> = res
        .hits
        .iter()
        .filter(|h| acl.allows(&c.who, h.path.as_bytes(), false))
        .map(|h| json!({ "path": h.path.to_string(), "line": h.line_number, "text": String::from_utf8_lossy(&h.line), "spans": h.spans }))
        .collect();
    reply(json!({ "changeset": id.to_hex(), "files_scanned": res.files_scanned, "truncated": res.truncated, "matches": hits }))
}

pub async fn find(c: &Ctx, repo: RepoId) -> ApiResult {
    c.flux.authorize(&c.who, repo, Role::Read).await?;
    let id = rev_or_default(c, repo, c.q("rev").unwrap_or("")).await?;
    let root = c.flux.root_tree(repo, &id).await?;
    let limit = c.num("limit", 50)?.clamp(1, 10_000) as usize;
    let paths = c.flux.find_files(root, c.q("q").unwrap_or(""), limit).await?;
    let acl = c.flux.repo_config(repo).await?;
    reply(paths.iter().filter(|p| acl.allows(&c.who, p.as_bytes(), false)).map(|p| p.to_string()).collect::<Vec<_>>())
}

pub async fn blame(c: &Ctx, repo: RepoId, rev: &str, path: &str) -> ApiResult {
    c.flux.authorize(&c.who, repo, Role::Read).await?;
    let id = rev_or_default(c, repo, rev).await?;
    let p = path_of(path)?;
    c.flux.check_read_path(repo, &c.who, p.as_bytes()).await?;
    let b = c.flux.blame(repo, &id, &p).await?;
    let mut ids: Vec<ChangesetId> = b.ranges.iter().map(|r| r.2).collect();
    ids.sort();
    ids.dedup();
    let loaded = c.flux.load_arc(repo, &ids).await?;
    let commits: BTreeMap<String, Value> = loaded
        .iter()
        .flatten()
        .map(|l| {
            let (n, e) = crate::hooks::split_sig(&l.changeset.author);
            (l.id.to_hex(), json!({ "author": n, "email": e, "time": l.changeset.author_time.unix_ms, "summary": String::from_utf8_lossy(&l.changeset.message).lines().next().unwrap_or("").to_string() }))
        })
        .collect();
    let ranges: Vec<Value> =
        b.ranges.iter().map(|(start, n, cs)| json!({ "start_line": start + 1, "lines": n, "changeset": cs.to_hex() })).collect();
    reply(json!({ "path": path, "lines": b.lines, "ranges": ranges, "commits": commits }))
}

// ───────────── agents: context and merge checks ─────────────

/// Everything an agent needs to orient itself in one call: description,
/// default branch and head, agent instructions (AGENTS.md / CLAUDE.md),
/// README, top-level layout, languages, recent changes, open work,
/// workflows and ownership.
pub async fn context(c: &Ctx, repo: RepoId, name: &str) -> ApiResult {
    c.flux.authorize(&c.who, repo, Role::Read).await?;
    let cfg = c.flux.repo_config(repo).await?;
    let branch = default_branch(&c.flux, repo).await?;
    let id = rev_or_default(c, repo, c.q("rev").unwrap_or("")).await?;
    let root = c.flux.root_tree(repo, &id).await?;
    let read_first = |names: &'static [&'static str]| {
        let flux = c.flux.clone();
        async move {
            for n in names {
                let Ok(p) = RepoPath::new(n.as_bytes().to_vec()) else { continue };
                if let Ok(Some((_, d))) = read_at(&flux, repo, &id, &p).await {
                    let text = String::from_utf8_lossy(&d[..d.len().min(16 * 1024)]).into_owned();
                    return Some(json!({ "path": n, "content": text, "truncated": d.len() > 16 * 1024 }));
                }
            }
            None
        }
    };
    let agents = read_first(&["AGENTS.md", "CLAUDE.md", ".github/copilot-instructions.md", ".cursorrules"]).await;
    let readme = read_first(&["README.md", "README", "readme.md", "README.rst", "README.txt"]).await;
    let top = c.flux.blobs.get_tree(&root).await?.map(|t| {
        t.entries()
            .iter()
            .map(|e| json!({ "name": String::from_utf8_lossy(e.name.as_bytes()).into_owned(), "type": if e.is_dir() { "dir" } else { "file" }, "size": e.size, "files": if e.is_dir() { Some(e.descendant_files) } else { None } }))
            .collect::<Vec<_>>()
    });
    let files = c.flux.file_list(root).await?;
    let mut langs: BTreeMap<&str, u64> = BTreeMap::new();
    for (p, _, size) in files.iter() {
        if let Some((l, _)) = crate::web_util::language(p) {
            *langs.entry(l).or_default() += size;
        }
    }
    let mut langs: Vec<(&str, u64)> = langs.into_iter().collect();
    langs.sort_by_key(|l| std::cmp::Reverse(l.1));
    let recent = c.flux.log_page(repo, &id, &[], None, true, 0, 10).await?;
    let mut changes = Vec::new();
    for l in &recent {
        let (n, _) = crate::hooks::split_sig(&l.changeset.author);
        changes.push(json!({ "id": l.id.to_hex(), "author": n, "time": l.changeset.author_time.unix_ms,
            "summary": String::from_utf8_lossy(&l.changeset.message).lines().next().unwrap_or("").to_string() }));
    }
    let workflows: Vec<String> = c.flux.workflow_files(repo, &id).await.unwrap_or_default().into_iter().map(|(p, _)| p).collect();
    let (open_issues, _) = c.flux.meta.issue_counts(repo).await?;
    let base = c.base_url();
    reply(json!({
        "repository": name,
        "description": cfg.description,
        "default_branch": branch,
        "changeset": id.to_hex(),
        "clone_url": format!("{base}/{name}.git"),
        "permission": c.flux.repo_role(&c.who, repo).await?.as_str(),
        "agent_instructions": agents,
        "readme": readme,
        "top_level": top,
        "file_count": files.len(),
        "languages": langs.iter().take(8).map(|(l, b)| json!({ "language": l, "bytes": b })).collect::<Vec<_>>(),
        "recent_changes": changes,
        "open_issues": open_issues,
        "open_reviews": c.flux.meta.review_count(repo, "open").await?,
        "workflows": workflows,
        "has_codeowners": !c.flux.codeowners(repo, &id).await?.is_empty(),
        "protection": c.flux.branch_rule(repo, branch.as_deref().unwrap_or("main").as_bytes()).await?,
        "api": {
            "read_files": format!("POST /api/v1/repos/{name}/files"),
            "write": format!("POST /api/v1/repos/{name}/changes"),
            "search": format!("GET /api/v1/repos/{name}/search?q="),
            "mcp": "/api/mcp",
        },
    }))
}

#[derive(Deserialize)]
struct MergeCheckReq {
    head: String,
    #[serde(default)]
    target: Option<String>,
}

/// Would `head` land cleanly on `target`? Computed without writing
/// anything: files both sides changed since the merge base are merged in
/// memory.
pub async fn merge_check(c: &Ctx, repo: RepoId) -> ApiResult {
    c.flux.authorize(&c.who, repo, Role::Read).await?;
    let r: MergeCheckReq = c.parse()?;
    let head = c.flux.resolve_rev(repo, &r.head).await?;
    let target_name = match r.target {
        Some(t) => t,
        None => default_branch(&c.flux, repo).await?.ok_or_else(|| bad("no target"))?,
    };
    let Some(target) = c.flux.meta.get_ref(repo, target_name.as_bytes()).await?.map(|v| v.target) else {
        return reply(json!({ "mergeable": true, "fast_forward": true, "conflicts": [] }));
    };
    if c.flux.is_ancestor(repo, &target, &head).await? {
        return reply(json!({ "mergeable": true, "fast_forward": true, "conflicts": [], "behind_by": 0 }));
    }
    let base = c.flux.merge_base(repo, &head, &target).await?;
    let (ahead, behind) = c.flux.ahead_behind(repo, &head, &target).await?;
    let tree = |id: Option<ChangesetId>| {
        let flux = c.flux.clone();
        async move {
            match id {
                Some(i) => flux.root_tree(repo, &i).await.map(Some),
                None => Ok(None),
            }
        }
    };
    let (tb, th, tt) = (tree(base).await?, tree(Some(head)).await?, tree(Some(target)).await?);
    let ours = flux_derive::diff_trees(&c.flux.blobs, tb, th, &|_, _| true).await?;
    let theirs = flux_derive::diff_trees(&c.flux.blobs, tb, tt, &|_, _| true).await?;
    let theirs: BTreeMap<String, &flux_derive::TreeDiffEntry> = theirs.iter().map(|d| (d.path.to_string(), d)).collect();
    let mut conflicts = Vec::new();
    let mut merged = 0;
    for d in &ours {
        let p = d.path.to_string();
        let Some(t) = theirs.get(&p) else { continue };
        let same = d.new.as_ref().map(|f| (f.kind, f.content_id)) == t.new.as_ref().map(|f| (f.kind, f.content_id));
        if same {
            continue;
        }
        let get = |f: &Option<flux_derive::TreeFile>| {
            let cid = f.as_ref().and_then(|f| f.content_id);
            let flux = c.flux.clone();
            async move {
                match cid {
                    Some(id) => flux.blobs.get_content_bytes(&id).await.ok().flatten(),
                    None => None,
                }
            }
        };
        let (b, o, th) = (get(&d.old).await, get(&d.new).await, get(&t.new).await);
        let clean = match (b, o, th) {
            (Some(b), Some(o), Some(th)) if ![&b, &o, &th].iter().any(|x| crate::web_util::is_binary(x)) => {
                diffy::merge_bytes(&b, &th, &o).is_ok()
            }
            _ => false,
        };
        if clean {
            merged += 1;
        } else {
            conflicts.push(p);
        }
    }
    reply(json!({
        "mergeable": conflicts.is_empty(),
        "fast_forward": false,
        "conflicts": conflicts,
        "auto_merged_files": merged,
        "ahead_by": ahead,
        "behind_by": behind,
        "merge_base": base.map(|b| b.to_hex()),
    }))
}

// ───────────── archives and events ─────────────

pub async fn archive(c: &Ctx, repo: RepoId, name: &str, file: &str) -> ApiResult {
    c.flux.authorize(&c.who, repo, Role::Read).await?;
    if c.flux.repo_config(repo).await?.has_rules() && c.flux.repo_role(&c.who, repo).await? < Role::Admin {
        return Err(super::ApiError(
            axum::http::StatusCode::FORBIDDEN,
            "archives are unavailable when path rules restrict reading".into(),
        ));
    }
    let (rev, format, ext) = if let Some(r) = file.strip_suffix(".tar.gz") {
        (r, crate::repos::ArchiveFormat::TarGz, "tar.gz")
    } else if let Some(r) = file.strip_suffix(".tgz") {
        (r, crate::repos::ArchiveFormat::TarGz, "tar.gz")
    } else if let Some(r) = file.strip_suffix(".zip") {
        (r, crate::repos::ArchiveFormat::Zip, "zip")
    } else {
        return Err(bad("use <rev>.tar.gz or <rev>.zip"));
    };
    let id = c.flux.resolve_rev(repo, rev).await?;
    let short = name.rsplit('/').next().unwrap_or(name);
    let safe_rev: String =
        rev.chars().map(|ch| if ch.is_ascii_alphanumeric() || "-_.".contains(ch) { ch } else { '-' }).collect();
    let prefix = format!("{short}-{safe_rev}");
    let data = c.flux.archive(repo, &id, &prefix, format).await?;
    let mut r = data.into_response();
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(if ext == "zip" { "application/zip" } else { "application/gzip" }));
    if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{prefix}.{ext}\"")) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    Ok(r)
}

/// Server-sent events for repositories the caller can read
/// (`?repo=name` narrows to one).
pub async fn events(c: &Ctx) -> ApiResult {
    use axum::response::sse::{Event as Sse, KeepAlive};
    let want = match c.q("repo") {
        Some(n) => {
            let id = c.flux.repo(n).await?;
            c.flux.authorize(&c.who, id, Role::Read).await?;
            Some(n.to_string())
        }
        None => None,
    };
    let mut rx = c.flux.subscribe();
    let flux: Arc<Flux> = c.flux.clone();
    let who = c.who.clone();
    let stream = async_stream(move |tx| async move {
        loop {
            let e = match rx.recv().await {
                Ok(e) => e,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    let _ = tx.send(Sse::default().event("lagged").data(n.to_string())).await;
                    continue;
                }
                Err(_) => return,
            };
            if want.as_deref().is_some_and(|w| w != e.repo()) {
                continue;
            }
            let Ok(id) = flux.repo(e.repo()).await else { continue };
            if flux.repo_role(&who, id).await.unwrap_or_default() < Role::Read {
                continue;
            }
            let pb = crate::grpc::event_pb(&e);
            let data = json!({
                "repo": pb.repo, "kind": pb.kind, "ref": String::from_utf8_lossy(&pb.ref_name),
                "old": hex::encode(&pb.old), "new": hex::encode(&pb.new), "actor": pb.actor,
                "count": pb.count, "number": pb.review, "detail": pb.detail,
            });
            if tx.send(Sse::default().event(pb.kind.clone()).data(data.to_string())).await.is_err() {
                return;
            }
        }
    });
    Ok(axum::response::Sse::new(stream).keep_alive(KeepAlive::default()).into_response())
}

/// A stream fed by a task through a bounded channel.
fn async_stream<T, F, Fut>(f: F) -> impl futures::Stream<Item = Result<T, std::convert::Infallible>>
where
    T: Send + 'static,
    F: FnOnce(tokio::sync::mpsc::Sender<T>) -> Fut,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let (tx, rx) = tokio::sync::mpsc::channel(64);
    tokio::spawn(f(tx));
    tokio_stream::wrappers::ReceiverStream::new(rx).map(Ok)
}
