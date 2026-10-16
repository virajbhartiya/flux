//! Bulk import of issues, reviews (pull requests), comments, labels,
//! milestones, releases and placeholder accounts — what `flux migrate
//! github` sends after importing the git history.
//!
//! Imports keep numbers, authors and timestamps, and are idempotent:
//! re-sending the same data (mirror syncs) updates mutable fields and adds
//! only new comments (matched by their source ids).

use std::collections::BTreeMap;

use flux_core::ChangesetId;
use flux_store::{ImportedReview, IssueUpdate, Label, NewIssue, RepoId};
use serde::Deserialize;
use serde_json::json;

use super::{ApiError, ApiResult, Ctx, bad, reply};
use crate::rbac::Role;

#[derive(Deserialize, Default)]
pub struct ImportBatch {
    #[serde(default)]
    pub users: Vec<UserIn>,
    #[serde(default)]
    pub labels: Vec<LabelIn>,
    #[serde(default)]
    pub milestones: Vec<MilestoneIn>,
    #[serde(default)]
    pub issues: Vec<IssueIn>,
    #[serde(default)]
    pub reviews: Vec<ReviewIn>,
    #[serde(default)]
    pub releases: Vec<ReleaseIn>,
}

#[derive(Deserialize)]
pub struct UserIn {
    pub login: String,
    #[serde(default)]
    pub name: String,
    /// Identity to link (`github`), so signing in with it claims the account.
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub subject: Option<String>,
}

#[derive(Deserialize)]
pub struct LabelIn {
    pub name: String,
    #[serde(default)]
    pub color: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Deserialize)]
pub struct MilestoneIn {
    pub number: i64,
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub due_on: Option<i64>,
}

#[derive(Deserialize)]
pub struct CommentIn {
    /// Source id (makes re-imports idempotent).
    pub id: String,
    pub author: String,
    pub body: String,
    pub created_at: i64,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub line: Option<i64>,
    /// For reviews: `comment`, `approved`, `changes_requested`.
    #[serde(default)]
    pub kind: Option<String>,
}

#[derive(Deserialize)]
pub struct IssueIn {
    pub number: i64,
    pub title: String,
    #[serde(default)]
    pub body: String,
    pub author: String,
    pub state: String,
    #[serde(default)]
    pub state_reason: String,
    pub created_at: i64,
    #[serde(default)]
    pub updated_at: Option<i64>,
    #[serde(default)]
    pub closed_at: Option<i64>,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub assignees: Vec<String>,
    #[serde(default)]
    pub milestone: Option<i64>,
    #[serde(default)]
    pub locked: bool,
    #[serde(default)]
    pub comments: Vec<CommentIn>,
}

#[derive(Deserialize)]
pub struct ReviewIn {
    pub number: i64,
    pub title: String,
    #[serde(default)]
    pub body: String,
    pub author: String,
    /// `open`, `closed` or `landed`.
    pub state: String,
    /// Head commit: git SHA-1 or changeset id.
    pub head: String,
    pub target: String,
    /// The commit it landed as (git SHA-1 or changeset id).
    #[serde(default)]
    pub merged: Option<String>,
    #[serde(default)]
    pub draft: bool,
    pub created_at: i64,
    #[serde(default)]
    pub updated_at: Option<i64>,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub assignees: Vec<String>,
    #[serde(default)]
    pub reviewers: Vec<String>,
    #[serde(default)]
    pub comments: Vec<CommentIn>,
}

#[derive(Deserialize)]
pub struct ReleaseIn {
    pub tag: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub prerelease: bool,
    #[serde(default)]
    pub author: String,
    pub created_at: i64,
    #[serde(default)]
    pub published_at: Option<i64>,
}

async fn resolve_commit(c: &Ctx, repo: RepoId, s: &str) -> Option<ChangesetId> {
    let s = s.trim();
    if s.len() == 40
        && let Ok(b) = hex::decode(s)
        && let Ok(sha) = <[u8; 20]>::try_from(b.as_slice())
    {
        if let Ok(v) = c.flux.meta.git_to_cs(repo, &[sha]).await
            && let Some(Some(cs)) = v.first()
        {
            return Some(*cs);
        }
        if let Ok(v) = c.flux.meta.git_commits_lookup(repo, &[sha]).await
            && let Some(Some(cs)) = v.first()
        {
            return Some(*cs);
        }
        return None;
    }
    c.flux.resolve_rev(repo, s).await.ok()
}

pub async fn import(c: &Ctx, repo: RepoId) -> ApiResult {
    c.flux.authorize(&c.who, repo, Role::Admin).await?;
    let b: ImportBatch = c.parse()?;
    let meta = &c.flux.meta;
    let mut stats: BTreeMap<&str, usize> = BTreeMap::new();

    if !b.users.is_empty() {
        c.who.require_admin()?;
    }
    for u in &b.users {
        if !crate::identity::valid_name(&u.login) {
            continue;
        }
        let user = match meta.get_user(&u.login).await? {
            Some(x) => x,
            None => {
                let created = meta
                    .create_account(&flux_store::NewAccount {
                        name: u.login.clone(),
                        email: None,
                        display_name: u.name.clone(),
                        password_hash: None,
                        is_admin: false,
                    })
                    .await?;
                meta.set_user_kind(created.id, "placeholder").await?;
                *stats.entry("users").or_default() += 1;
                created
            }
        };
        if let (Some(p), Some(s)) = (&u.provider, &u.subject)
            && meta.identity(p, s).await?.is_none()
        {
            let _ = meta.link_identity(p, s, user.id, &u.login).await;
        }
    }

    for l in &b.labels {
        let color = l.color.trim_start_matches('#').to_ascii_lowercase();
        let color = if crate::issues::valid_color(&color) { color } else { "8a8f98".into() };
        meta.put_label(repo, &Label { name: l.name.clone(), color, description: l.description.clone() }).await?;
        *stats.entry("labels").or_default() += 1;
    }

    let existing: Vec<i64> = meta.list_milestones(repo).await?.iter().map(|m| m.number).collect();
    for m in &b.milestones {
        if !existing.contains(&m.number) {
            meta.create_milestone(repo, Some(m.number), &m.title, &m.description, m.due_on).await?;
        }
        let state = if m.state == "closed" { "closed" } else { "open" };
        meta.update_milestone(repo, m.number, &m.title, &m.description, state, m.due_on).await?;
        *stats.entry("milestones").or_default() += 1;
    }

    for i in &b.issues {
        match meta.item_kind(repo, i.number).await? {
            Some("review") => return Err(bad(format!("#{} is already a review", i.number))),
            Some(_) => {
                meta.update_issue(
                    repo,
                    i.number,
                    &IssueUpdate {
                        title: Some(i.title.clone()),
                        body: Some(i.body.clone()),
                        state: Some(if i.state == "closed" { "closed" } else { "open" }.into()),
                        state_reason: Some(i.state_reason.clone()),
                        milestone: Some(i.milestone),
                        locked: Some(i.locked),
                    },
                )
                .await?;
            }
            None => {
                meta.create_issue(
                    repo,
                    &NewIssue {
                        number: Some(i.number),
                        title: i.title.clone(),
                        body: i.body.clone(),
                        author: i.author.clone(),
                        state: if i.state == "closed" { "closed".into() } else { "open".into() },
                        state_reason: i.state_reason.clone(),
                        milestone: i.milestone,
                        created_ms: Some(i.created_at),
                        updated_ms: i.updated_at,
                        closed_ms: i.closed_at,
                    },
                )
                .await?;
                if i.locked {
                    meta.update_issue(repo, i.number, &IssueUpdate { locked: Some(true), ..Default::default() }).await?;
                }
            }
        }
        meta.set_item_labels(repo, i.number, &i.labels).await?;
        meta.set_item_people(repo, i.number, "assignee", &i.assignees).await?;
        for cm in &i.comments {
            meta.add_issue_event(
                repo,
                i.number,
                "comment",
                &cm.author,
                &cm.body,
                Some(cm.created_at),
                Some(&format!("import:{}", cm.id)),
            )
            .await?;
        }
        *stats.entry("issues").or_default() += 1;
    }

    let mut unresolved = Vec::new();
    for r in &b.reviews {
        if meta.item_kind(repo, r.number).await? == Some("issue") {
            return Err(bad(format!("#{} is already an issue", r.number)));
        }
        let merged = match &r.merged {
            Some(m) => resolve_commit(c, repo, m).await,
            None => None,
        };
        let target_head = meta.get_ref(repo, r.target.as_bytes()).await?.map(|v| v.target);
        let head = match resolve_commit(c, repo, &r.head).await.or(merged).or(target_head) {
            Some(h) => h,
            None => {
                unresolved.push(r.number);
                continue;
            }
        };
        let state = match r.state.as_str() {
            "landed" | "merged" => "landed",
            "closed" => "closed",
            _ => "open",
        };
        meta.import_review(
            repo,
            &ImportedReview {
                number: r.number,
                title: r.title.clone(),
                body: r.body.clone(),
                author: r.author.clone(),
                head,
                target: r.target.as_bytes().to_vec(),
                state: state.into(),
                landed: if state == "landed" { merged.or(Some(head)) } else { None },
                draft: r.draft,
                created_ms: r.created_at,
                updated_ms: r.updated_at.unwrap_or(r.created_at),
            },
        )
        .await?;
        meta.import_review_event(
            repo,
            r.number,
            "opened",
            &r.author,
            "",
            None,
            None,
            r.created_at,
            &format!("import:open:{}", r.number),
        )
        .await?;
        for cm in &r.comments {
            let kind = match cm.kind.as_deref() {
                Some("approved") => "approved",
                Some("changes_requested") => "changes_requested",
                _ => "comment",
            };
            meta.import_review_event(
                repo,
                r.number,
                kind,
                &cm.author,
                &cm.body,
                cm.path.as_deref().map(str::as_bytes),
                cm.line,
                cm.created_at,
                &format!("import:{}", cm.id),
            )
            .await?;
        }
        if state == "landed" {
            meta.import_review_event(
                repo,
                r.number,
                "landed",
                &r.author,
                "imported",
                None,
                None,
                r.updated_at.unwrap_or(r.created_at),
                &format!("import:landed:{}", r.number),
            )
            .await?;
        }
        meta.set_item_labels(repo, r.number, &r.labels).await?;
        meta.set_item_people(repo, r.number, "assignee", &r.assignees).await?;
        meta.set_item_people(repo, r.number, "reviewer", &r.reviewers).await?;
        *stats.entry("reviews").or_default() += 1;
    }

    for rel in &b.releases {
        let target = meta.get_ref(repo, format!("tags/{}", rel.tag).as_bytes()).await?.map(|v| v.target);
        let release = flux_store::Release {
            id: 0,
            tag: rel.tag.clone(),
            name: if rel.name.is_empty() { rel.tag.clone() } else { rel.name.clone() },
            body: rel.body.clone(),
            draft: rel.draft,
            prerelease: rel.prerelease,
            author: rel.author.clone(),
            target,
            created_ms: rel.created_at,
            published_ms: rel.published_at,
        };
        match meta.release_by_tag(repo, &rel.tag).await? {
            Some(cur) => {
                meta.update_release(repo, &flux_store::Release { id: cur.id, ..release }).await?;
            }
            None => {
                meta.create_release(repo, &release).await?;
            }
        }
        *stats.entry("releases").or_default() += 1;
    }

    let name = c.flux.repo_name(repo).await;
    c.flux.audit(&c.who, "repo.import", &name, &serde_json::to_string(&stats).unwrap_or_default()).await;
    if !unresolved.is_empty() && stats.get("reviews").copied().unwrap_or(0) == 0 && b.issues.is_empty() {
        return Err(ApiError(
            axum::http::StatusCode::CONFLICT,
            format!("no commits found for reviews {unresolved:?}; import the git history first"),
        ));
    }
    reply(json!({ "imported": stats, "unresolved_reviews": unresolved }))
}
