//! Migrate GitHub repositories to flux (`flux migrate github`, `flux mirror`).
//!
//! A migration mirrors the git repository into a local cache and imports
//! its history (commit SHA-1s preserved), then sends users, labels,
//! milestones, issues, pull requests (as reviews), releases and branch
//! protection through the bulk import API. Every step is idempotent, and
//! re-running syncs incrementally: `since=` for issues and comments, ETags
//! for small lists, and the git importer skips known commits.

pub mod audit;
pub mod convert;
pub mod flux;
pub mod gitcache;
pub mod github;
pub mod model;
pub mod report;
pub mod state;
pub mod time;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use flux_proto::v1::update_ref_request::Expect;
use futures::StreamExt as _;
use serde_json::json;

use crate::convert::{Converter, ImportBatch, Links, UserIn};
use crate::flux::FluxApi;
pub use crate::github::GitHub;
use crate::github::status_of;
use crate::model::{Issue, IssueComment, Label, Milestone, Profile, Pull, Release, Repo, Review, ReviewComment, User};
pub use crate::report::Report;
use crate::state::State;
use crate::time::parse_rfc3339;

/// Items per bulk import request.
const BATCH: usize = 200;
/// The largest release asset flux accepts.
const MAX_ASSET: u64 = 512 << 20;
/// Incremental fetches start this much before the last sync (clock skew,
/// items updated while the last sync ran).
const OVERLAP_MS: i64 = 60_000;

/// One repository migration. The flux token is deliberately not `Debug`.
#[derive(Clone)]
pub struct Options {
    /// `owner/repo` on GitHub.
    pub github_repo: String,
    /// flux server URL.
    pub server: String,
    pub token: Option<String>,
    /// flux repository name (`owner/name` or `name`).
    pub flux_repo: String,
    /// Visibility for a new flux repository (default: GitHub's).
    pub visibility: Option<String>,
    pub issues: bool,
    pub reviews: bool,
    pub releases: bool,
    pub protection: bool,
    /// Create placeholder accounts linked to GitHub identities (site admin).
    pub create_users: bool,
    pub rewrite_links: bool,
    /// Root of the git mirror cache (default `~/.cache/flux/migrate`).
    pub cache_dir: Option<PathBuf>,
    /// Fetch git from here instead of GitHub's clone URL.
    pub clone_url: Option<String>,
    /// Keeping a mirror in sync (GitHub stays the source of truth).
    pub mirror: bool,
}

impl Options {
    pub fn new(github_repo: &str, server: &str, flux_repo: &str) -> Self {
        Self {
            github_repo: github_repo.into(),
            server: server.into(),
            token: None,
            flux_repo: flux_repo.into(),
            visibility: None,
            issues: true,
            reviews: true,
            releases: true,
            protection: true,
            create_users: false,
            rewrite_links: true,
            cache_dir: None,
            clone_url: None,
            mirror: false,
        }
    }
}

/// `owner/repo` from `owner/repo`, `https://github.com/owner/repo(.git)` or
/// `git@github.com:owner/repo.git`.
pub fn split_repo(s: &str) -> Result<(String, String)> {
    let s = s.trim().trim_end_matches('/').trim_end_matches(".git");
    let path = match s.split_once("://") {
        Some((_, rest)) => rest.split_once('/').map_or("", |x| x.1),
        None => s.rsplit_once(':').map_or(s, |x| x.1),
    };
    match path.split_once('/') {
        Some((o, r)) if !o.is_empty() && !r.is_empty() && !r.contains('/') => Ok((o.to_string(), r.to_string())),
        _ => bail!("expected a GitHub repository as owner/repo, got {s:?}"),
    }
}

/// Every repository of an organization (or user).
pub async fn org_repos(gh: &GitHub, org: &str) -> Result<Vec<Repo>> {
    match gh.list(&format!("/orgs/{org}/repos?type=all")).await {
        Err(e) if status_of(&e) == Some(404) => gh.list(&format!("/users/{org}/repos?type=owner")).await,
        r => r,
    }
}

/// refs/heads/x → x, refs/tags/x → tags/x, refs/pull/1/head → pull/1/head.
fn flux_ref(git: &str) -> String {
    git.strip_prefix("refs/heads/").or_else(|| git.strip_prefix("refs/")).unwrap_or(git).to_string()
}

/// GitHub users seen in the migrated data: login → id (bots and deleted
/// accounts excluded).
#[derive(Default)]
struct Seen(BTreeMap<String, u64>);

impl Seen {
    fn add<'a>(&mut self, users: impl IntoIterator<Item = &'a User>) {
        for u in users {
            if !u.login.is_empty() && u.id != 0 && u.kind != "Bot" {
                self.0.insert(u.login.clone(), u.id);
            }
        }
    }
}

/// Lists that are absent when a feature is off on GitHub.
async fn list_or_empty<T: serde::de::DeserializeOwned>(gh: &GitHub, path: &str) -> Result<Vec<T>> {
    match gh.list(path).await {
        Err(e) if matches!(status_of(&e), Some(404 | 410)) => Ok(vec![]),
        r => r,
    }
}

/// Send `items` in batches; `put` places a batch into the request.
async fn upload<T>(
    flux: &FluxApi,
    repo: &str,
    kind: &str,
    items: Vec<T>,
    put: fn(&mut ImportBatch, Vec<T>),
    report: &mut Report,
) -> Result<()> {
    let total = items.len() as u64;
    let mut it = items.into_iter();
    loop {
        let chunk: Vec<T> = it.by_ref().take(BATCH).collect();
        if chunk.is_empty() {
            break;
        }
        let mut b = ImportBatch::default();
        put(&mut b, chunk);
        let v = flux.import(repo, &b).await.with_context(|| format!("importing {kind}"))?;
        for n in v["unresolved_reviews"].as_array().into_iter().flatten() {
            report.warn(format!("pull request #{n} was skipped: none of its commits are in flux"));
        }
    }
    report.add(kind, total);
    Ok(())
}

async fn import_git(git_dir: &Path, flux: &FluxApi, flux_repo: &str, st: &mut State, report: &mut Report) -> Result<()> {
    let patterns: Vec<String> = gitcache::REF_PATTERNS.iter().map(|p| p.to_string()).collect();
    let refs = {
        let (dir, pats) = (git_dir.to_path_buf(), patterns.clone());
        tokio::task::spawn_blocking(move || flux_git::git::list_refs(&dir, &pats)).await??
    };
    let now: BTreeMap<String, String> = refs.iter().map(|(n, sha)| (flux_ref(n), hex(sha))).collect();
    let client = flux_client::Client::connect_with_token(flux.server(), flux_repo, flux.token()).await?;
    if !refs.is_empty() {
        let mut io = flux_git::ImportOptions::new(git_dir);
        io.ref_patterns = patterns;
        let stats = flux_git::import(&client, &io).await.context("importing git history")?;
        report.add("commits", stats.commits_imported as u64);
        report.add("refs", now.iter().filter(|(n, sha)| st.refs.get(*n) != Some(sha)).count() as u64);
    } else {
        report.warn("the GitHub repository has no commits");
    }
    for gone in st.refs.keys().filter(|n| !now.contains_key(*n)) {
        match client.update_ref(gone, Expect::Any(true), None, "deleted on GitHub").await {
            Ok(_) => report.add("refs_deleted", 1),
            Err(e) => report.warn(format!("could not delete ref {gone} (deleted on GitHub): {e:#}")),
        }
    }
    st.refs = now;
    Ok(())
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Migrate (or re-sync) one repository.
pub async fn migrate(gh: &GitHub, opts: &Options) -> Result<Report> {
    let started = Instant::now();
    let requests = gh.requests();
    let (owner, name) = split_repo(&opts.github_repo)?;
    let flux = FluxApi::new(&opts.server, opts.token.clone())?;
    let mut report =
        Report { github: format!("{}/{owner}/{name}", gh.host()), flux_repo: opts.flux_repo.clone(), ..Default::default() };
    let root = opts.cache_dir.clone().or_else(gitcache::default_root).context("no cache directory; pass --cache-dir")?;
    let git_dir = gitcache::repo_dir(&root, &gh.host(), &owner, &name);
    let state_path = state::path_for(&git_dir);
    let mut st = State::load(&state_path, &format!("{} {}", flux.server(), opts.flux_repo));
    gh.load_cache(std::mem::take(&mut st.etags));
    report.incremental = st.since.is_some();
    let base = format!("/repos/{owner}/{name}");

    // ── the repository ──
    let t = Instant::now();
    let repo: Repo = gh.get(&base).await.with_context(|| format!("reading {owner}/{name} on GitHub"))?;
    let sync_from = gh.server_time_ms().unwrap_or_else(time::now_ms) - OVERLAP_MS;
    let visibility = opts.visibility.clone().unwrap_or_else(|| repo.visibility().to_string());
    let description = repo.description.clone().unwrap_or_default();
    let (info, created) = flux.ensure_repo(&opts.flux_repo, &visibility, &description, &repo.default_branch).await?;
    report.created = created;
    if info["archived"].as_bool() == Some(true) {
        flux.patch_repo(&opts.flux_repo, &json!({ "archived": false })).await?;
    }
    let mut patch = json!({
        "description": description,
        "homepage": repo.homepage.clone().unwrap_or_default(),
        "topics": repo.topics,
        "has_issues": repo.has_issues,
    });
    if !repo.default_branch.is_empty() {
        patch["default_branch"] = json!(repo.default_branch);
    }
    let info = flux.patch_repo(&opts.flux_repo, &patch).await?;
    report.flux_url = info["html_url"].as_str().unwrap_or_default().to_string();
    report.clone_url = info["clone_url"].as_str().unwrap_or_default().to_string();
    report.phase("repository", t);

    // ── git ──
    let t = Instant::now();
    let clone = opts.clone_url.clone().unwrap_or_else(|| repo.clone_url.clone());
    tracing::info!("fetching git into {}", git_dir.display());
    gitcache::sync(&git_dir, &clone, gh.token()).await.context("mirroring the git repository")?;
    tracing::info!("importing git history");
    import_git(&git_dir, &flux, &opts.flux_repo, &mut st, &mut report).await?;
    report.phase("git", t);

    // ── fetch metadata ──
    let t = Instant::now();
    tracing::info!("fetching {} from GitHub", if report.incremental { "changes" } else { "issues, pull requests and releases" });
    let conv = Converter { links: opts.rewrite_links.then(|| Links::new(&repo.html_url, &report.flux_url)) };
    let since_q = st.since.as_deref().map(|s| format!("&since={s}")).unwrap_or_default();
    let since_ms = st.since.as_deref().and_then(parse_rfc3339);
    let mut seen = Seen::default();
    let mut out = ImportBatch::default();

    let (labels, changed) = gh.list_cached::<Label>(&format!("{base}/labels")).await?;
    if changed {
        out.labels = labels.iter().map(|l| conv.label(l)).collect();
    }
    let (milestones, changed) = gh.list_cached::<Milestone>(&format!("{base}/milestones?state=all")).await?;
    if changed {
        out.milestones = milestones.iter().map(|m| conv.milestone(m)).collect();
    }

    let mut comments = 0u64;
    let mut heads: Vec<(i64, String, String)> = Vec::new();
    if opts.issues || opts.reviews {
        let items: Vec<Issue> =
            list_or_empty(gh, &format!("{base}/issues?state=all&sort=updated&direction=asc{since_q}")).await?;
        let (issues, _) = convert::split_issues(items);
        let conversation: Vec<IssueComment> =
            list_or_empty(gh, &format!("{base}/issues/comments?sort=created&direction=asc{since_q}")).await?;
        for c in &conversation {
            seen.add(&c.user);
        }
        let mut conversation = convert::by_number(conversation, |c| c.issue_url.as_str());
        if opts.issues {
            for i in &issues {
                seen.add(i.user.iter().chain(&i.assignees));
                let cs = conversation.remove(&i.number).unwrap_or_default();
                comments += cs.len() as u64;
                out.issues.push(conv.issue(i, &cs));
            }
        }
        if opts.reviews {
            let pulls: Vec<Pull> = gh
                .list_until(&format!("{base}/pulls?state=all&sort=updated&direction=desc"), |p: &Pull| {
                    since_ms.is_none_or(|s| parse_rfc3339(&p.updated_at).is_none_or(|u| u >= s))
                })
                .await?;
            let lines: Vec<ReviewComment> =
                gh.list(&format!("{base}/pulls/comments?sort=created&direction=asc{since_q}")).await?;
            for c in &lines {
                seen.add(&c.user);
            }
            let mut lines = convert::by_number(lines, |c| c.pull_request_url.as_str());
            let mut pulls = pulls;
            pulls.sort_by_key(|p| p.number);
            for p in &pulls {
                let reviews: Vec<Review> = gh.list(&format!("{base}/pulls/{}/reviews", p.number)).await?;
                seen.add(p.user.iter().chain(&p.assignees).chain(&p.requested_reviewers));
                for r in &reviews {
                    seen.add(&r.user);
                }
                let conv_comments = conversation.remove(&p.number).unwrap_or_default();
                let line_comments = lines.remove(&p.number).unwrap_or_default();
                let r = conv.review(p, &conv_comments, &line_comments, &reviews);
                comments += r.comments.len() as u64;
                if !r.head.is_empty() {
                    heads.push((p.number, r.head.clone(), r.target.clone()));
                }
                out.reviews.push(r);
            }
        }
    }

    let mut releases: Vec<Release> = Vec::new();
    if opts.releases {
        let (all, changed) = gh.list_cached::<Release>(&format!("{base}/releases")).await?;
        if changed {
            for r in &all {
                seen.add(&r.author);
            }
            out.releases = all.iter().map(|r| conv.release(r)).collect();
            releases = all;
        }
    }

    if opts.create_users {
        match gh.list_cached::<User>(&format!("{base}/collaborators?affiliation=all")).await {
            Ok((c, _)) => seen.add(&c),
            Err(e) if matches!(status_of(&e), Some(403 | 404)) => {
                if !report.incremental {
                    report.warn("could not list collaborators (needs push access); placeholder accounts cover active users only");
                }
            }
            Err(e) => return Err(e),
        }
        for (login, id) in std::mem::take(&mut seen.0) {
            if st.users.contains_key(&login) {
                continue;
            }
            let name = gh.get_opt::<Profile>(&format!("/users/{login}")).await.ok().flatten().and_then(|p| p.name);
            out.users.push(UserIn {
                login,
                name: name.unwrap_or_default(),
                provider: Some("github".into()),
                subject: Some(id.to_string()),
            });
        }
    }
    report.phase("fetch", t);

    let missing = gitcache::missing_commits(&git_dir, &heads.iter().map(|h| h.1.clone()).collect::<Vec<_>>()).await?;
    for (n, head, target) in &heads {
        if missing.contains(head) {
            report.warn(format!(
                "pull request #{n}: head commit {} is not in the repository (deleted fork?); imported against {target}",
                &head[..head.len().min(12)]
            ));
        }
    }

    // ── upload, in dependency order ──
    let t = Instant::now();
    tracing::info!(issues = out.issues.len(), reviews = out.reviews.len(), releases = out.releases.len(), "uploading to flux");
    let r = &opts.flux_repo;
    let new_users: Vec<(String, u64)> =
        out.users.iter().map(|u| (u.login.clone(), u.subject.as_deref().and_then(|s| s.parse().ok()).unwrap_or(0))).collect();
    upload(&flux, r, "users", std::mem::take(&mut out.users), |b, v| b.users = v, &mut report)
        .await
        .context("placeholder accounts need a site admin token (or drop --create-users)")?;
    st.users.extend(new_users);
    upload(&flux, r, "labels", std::mem::take(&mut out.labels), |b, v| b.labels = v, &mut report).await?;
    upload(&flux, r, "milestones", std::mem::take(&mut out.milestones), |b, v| b.milestones = v, &mut report).await?;
    upload(&flux, r, "issues", std::mem::take(&mut out.issues), |b, v| b.issues = v, &mut report).await?;
    upload(&flux, r, "reviews", std::mem::take(&mut out.reviews), |b, v| b.reviews = v, &mut report).await?;
    report.add("comments", comments);
    upload(&flux, r, "releases", std::mem::take(&mut out.releases), |b, v| b.releases = v, &mut report).await?;
    // Assets move GitHub → flux eight at a time (each is a download and an
    // upload; doing them one by one leaves both links idle most of the time).
    let mut transfers = Vec::new();
    for rel in &releases {
        if rel.assets.is_empty() {
            continue;
        }
        let Some((id, have)) = flux.release(r, &rel.tag_name).await? else { continue };
        for a in rel.assets.iter().filter(|a| !have.contains(&a.name)) {
            if a.size > MAX_ASSET {
                report.warn(format!("release {}: asset {} is larger than 512 MiB; upload it by hand", rel.tag_name, a.name));
                continue;
            }
            let flux = &flux;
            transfers.push(async move {
                let data = gh.download(&a.url).await.with_context(|| format!("downloading {}", a.name))?;
                flux.upload_asset(r, id, &a.name, &a.content_type, data).await
            });
        }
    }
    let mut pending = futures::stream::iter(transfers).buffer_unordered(8);
    while let Some(done) = futures::StreamExt::next(&mut pending).await {
        done?;
        report.add("assets", 1);
    }
    report.phase("upload", t);

    // ── settings ──
    let t = Instant::now();
    if opts.protection && !repo.default_branch.is_empty() {
        protect(gh, &flux, &base, &repo.default_branch, opts, &mut report).await?;
    }
    if !report.incremental {
        audit::audit(gh, &repo, &opts.flux_repo, &git_dir, &mut report).await?;
    }
    if repo.archived {
        flux.patch_repo(&opts.flux_repo, &json!({ "archived": true })).await?;
        if !report.incremental {
            report.warn("the GitHub repository is archived; the flux repository was archived too");
        }
    }
    report.phase("settings", t);

    st.since = Some(time::format_rfc3339(sync_from));
    st.etags = gh.take_cache();
    st.save(&state_path)?;
    report.github_requests = gh.requests() - requests;
    report.elapsed_ms = started.elapsed().as_millis() as u64;
    Ok(report)
}

/// Copy the default branch's protection into flux's rule for that branch.
async fn protect(gh: &GitHub, flux: &FluxApi, base: &str, branch: &str, opts: &Options, report: &mut Report) -> Result<()> {
    let path = format!("{base}/branches/{}/protection", branch.replace('/', "%2F"));
    let p = match gh.get_opt::<serde_json::Value>(&path).await {
        Ok(Some(p)) => p,
        Ok(None) => return Ok(()),
        Err(e) if status_of(&e) == Some(403) => {
            if !report.incremental {
                report.warn(format!(
                    "could not read branch protection for {branch} (needs admin access); set it with PUT /api/v1/repos/{}/protection",
                    opts.flux_repo
                ));
            }
            return Ok(());
        }
        Err(e) => return Err(e),
    };
    let mut rule = convert::protection_rule(branch, &p);
    if opts.mirror && rule.enforce_admins {
        // The mirror pushes as an admin; GitHub stays the gatekeeper.
        rule.enforce_admins = false;
        if !report.incremental {
            report.warn(format!(
                "enforce_admins on {branch} is off while mirroring; run `flux migrate github` at cutover to turn it on"
            ));
        }
    }
    if p["restrictions"].is_object() && !report.incremental {
        report.warn(format!("push restrictions on {branch} are not migrated; use push_allow in the flux branch rule"));
    }
    let current = flux.protection(&opts.flux_repo).await?;
    let before = current.as_ref().and_then(|rules| rules.iter().find(|r| r.pattern == branch).cloned());
    let rules = convert::upsert_rule(current, rule);
    if before.as_ref() != rules.iter().find(|r| r.pattern == branch) {
        flux.set_protection(&opts.flux_repo, &rules).await?;
        report.add("protection_rules", 1);
    }
    Ok(())
}

/// What to do after a migration, for people.
pub fn next_steps(report: &Report) -> Vec<String> {
    let mut steps = Vec::new();
    if !report.clone_url.is_empty() {
        steps.push(format!("git remote set-url origin {}", report.clone_url));
    }
    if !report.secrets.is_empty() {
        steps.push(format!("re-enter Actions secrets on flux: {}", report.secrets.join(", ")));
    }
    if report.warnings.iter().any(|w| w.starts_with("webhook ")) {
        steps.push("recreate the webhooks listed above".into());
    }
    if !report.flux_url.is_empty() {
        steps.push(format!("review the repository at {}", report.flux_url));
    }
    steps
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_names() {
        let want = ("acme".to_string(), "widget".to_string());
        for s in [
            "acme/widget",
            "https://github.com/acme/widget",
            "https://github.com/acme/widget.git",
            "git@github.com:acme/widget.git",
        ] {
            assert_eq!(split_repo(s).unwrap(), want, "{s}");
        }
        assert!(split_repo("widget").is_err());
        assert!(split_repo("a/b/c").is_err());
    }

    #[test]
    fn ref_names() {
        assert_eq!(flux_ref("refs/heads/feature/x"), "feature/x");
        assert_eq!(flux_ref("refs/tags/v1"), "tags/v1");
        assert_eq!(flux_ref("refs/pull/3/head"), "pull/3/head");
    }
}
