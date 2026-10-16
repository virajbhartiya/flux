//! [`CollabStore`] for both SQL backends (same queries, `$N` placeholders).

use async_trait::async_trait;
use flux_core::{ChangesetId, ContentId};
use sqlx::Row;

use super::{IN_CHUNK, cs_id, now_ms, placeholders, review_row};
use super::{PostgresStore, SqliteStore};
use crate::collab::*;
use crate::meta::*;

/// A dynamically bound query argument.
enum Arg {
    I(i64),
    S(String),
}

const ISSUE_COLS: &str =
    "number, title, body, author, state, state_reason, milestone, locked, comments, created_at, updated_at, closed_at";

fn content(b: &[u8]) -> StoreResult<ContentId> {
    ContentId::from_slice(b).map_err(|e| StoreError::Corrupt(format!("bad content id: {e}")))
}

fn words(s: &str) -> Vec<String> {
    s.split_whitespace().map(str::to_string).collect()
}

macro_rules! collab_store {
    ($name:ident, $db:ty) => {
        impl $name {
            fn issue(r: &<$db as sqlx::Database>::Row) -> StoreResult<Issue> {
                Ok(Issue {
                    number: r.try_get(0)?,
                    title: r.try_get(1)?,
                    body: r.try_get(2)?,
                    author: r.try_get(3)?,
                    state: r.try_get(4)?,
                    state_reason: r.try_get(5)?,
                    milestone: r.try_get(6)?,
                    locked: r.try_get(7)?,
                    comments: r.try_get(8)?,
                    created_ms: r.try_get(9)?,
                    updated_ms: r.try_get(10)?,
                    closed_ms: r.try_get(11)?,
                })
            }

            fn issue_event(r: &<$db as sqlx::Database>::Row) -> StoreResult<IssueEvent> {
                Ok(IssueEvent {
                    seq: r.try_get(0)?,
                    number: r.try_get(1)?,
                    kind: r.try_get(2)?,
                    author: r.try_get(3)?,
                    body: r.try_get(4)?,
                    created_ms: r.try_get(5)?,
                    updated_ms: r.try_get(6)?,
                })
            }

            fn release(r: &<$db as sqlx::Database>::Row) -> StoreResult<Release> {
                Ok(Release {
                    id: r.try_get(0)?,
                    tag: r.try_get(1)?,
                    name: r.try_get(2)?,
                    body: r.try_get(3)?,
                    draft: r.try_get(4)?,
                    prerelease: r.try_get(5)?,
                    author: r.try_get(6)?,
                    target: r.try_get::<Option<Vec<u8>>, _>(7)?.map(|b| cs_id(&b)).transpose()?,
                    created_ms: r.try_get(8)?,
                    published_ms: r.try_get(9)?,
                })
            }

            fn asset(r: &<$db as sqlx::Database>::Row) -> StoreResult<ReleaseAsset> {
                Ok(ReleaseAsset {
                    id: r.try_get(0)?,
                    release_id: r.try_get(1)?,
                    name: r.try_get(2)?,
                    content: content(&r.try_get::<Vec<u8>, _>(3)?)?,
                    size: r.try_get(4)?,
                    content_type: r.try_get(5)?,
                    downloads: r.try_get(6)?,
                    created_ms: r.try_get(7)?,
                })
            }

            fn status(r: &<$db as sqlx::Database>::Row, o: usize) -> StoreResult<CommitStatus> {
                Ok(CommitStatus {
                    context: r.try_get(o)?,
                    state: r.try_get(o + 1)?,
                    description: r.try_get(o + 2)?,
                    target_url: r.try_get(o + 3)?,
                    creator: r.try_get(o + 4)?,
                    created_ms: r.try_get(o + 5)?,
                    updated_ms: r.try_get(o + 6)?,
                })
            }

            fn webhook(r: &<$db as sqlx::Database>::Row) -> StoreResult<Webhook> {
                Ok(Webhook {
                    id: r.try_get(0)?,
                    scope_kind: r.try_get(1)?,
                    scope_id: r.try_get(2)?,
                    url: r.try_get(3)?,
                    secret: r.try_get(4)?,
                    events: words(&r.try_get::<String, _>(5)?),
                    active: r.try_get(6)?,
                    created_ms: r.try_get(7)?,
                })
            }

            fn delivery(r: &<$db as sqlx::Database>::Row) -> StoreResult<Delivery> {
                Ok(Delivery {
                    id: r.try_get(0)?,
                    hook_id: r.try_get(1)?,
                    event: r.try_get(2)?,
                    guid: r.try_get(3)?,
                    payload: r.try_get(4)?,
                    state: r.try_get(5)?,
                    attempts: r.try_get(6)?,
                    next_ms: r.try_get(7)?,
                    status_code: r.try_get(8)?,
                    response: r.try_get(9)?,
                    duration_ms: r.try_get(10)?,
                    created_ms: r.try_get(11)?,
                })
            }

            fn ssh_key(r: &<$db as sqlx::Database>::Row) -> StoreResult<SshKey> {
                Ok(SshKey {
                    id: r.try_get(0)?,
                    user_id: r.try_get(1)?,
                    title: r.try_get(2)?,
                    fingerprint: r.try_get(3)?,
                    public_key: r.try_get(4)?,
                    created_ms: r.try_get(5)?,
                    last_used: r.try_get(6)?,
                })
            }

            fn milestone(r: &<$db as sqlx::Database>::Row) -> StoreResult<Milestone> {
                Ok(Milestone {
                    number: r.try_get(0)?,
                    title: r.try_get(1)?,
                    description: r.try_get(2)?,
                    state: r.try_get(3)?,
                    due_ms: r.try_get(4)?,
                    created_ms: r.try_get(5)?,
                    closed_ms: r.try_get(6)?,
                    open_items: r.try_get(7)?,
                    closed_items: r.try_get(8)?,
                })
            }

            fn identity_row(r: &<$db as sqlx::Database>::Row) -> StoreResult<Identity> {
                Ok(Identity {
                    provider: r.try_get(0)?,
                    subject: r.try_get(1)?,
                    user_id: r.try_get(2)?,
                    login: r.try_get(3)?,
                    created_ms: r.try_get(4)?,
                })
            }

            fn ci_run(r: &<$db as sqlx::Database>::Row) -> StoreResult<CiRun> {
                Ok(CiRun {
                    id: r.try_get(0)?,
                    repo_id: RepoId(r.try_get(1)?),
                    workflow: r.try_get(2)?,
                    event: r.try_get(3)?,
                    ref_name: r.try_get(4)?,
                    cs: cs_id(&r.try_get::<Vec<u8>, _>(5)?)?,
                    review: r.try_get(6)?,
                    state: r.try_get(7)?,
                    actor: r.try_get(8)?,
                    created_ms: r.try_get(9)?,
                    updated_ms: r.try_get(10)?,
                })
            }

            fn ci_job(r: &<$db as sqlx::Database>::Row) -> StoreResult<CiJob> {
                Ok(CiJob {
                    id: r.try_get(0)?,
                    run_id: r.try_get(1)?,
                    repo_id: RepoId(r.try_get(2)?),
                    name: r.try_get(3)?,
                    needs: words(&r.try_get::<String, _>(4)?),
                    labels: words(&r.try_get::<String, _>(5)?),
                    spec: r.try_get(6)?,
                    state: r.try_get(7)?,
                    runner: r.try_get(8)?,
                    lease_until: r.try_get(9)?,
                    started_ms: r.try_get(10)?,
                    finished_ms: r.try_get(11)?,
                    exit_code: r.try_get(12)?,
                })
            }

            fn ci_runner(r: &<$db as sqlx::Database>::Row) -> StoreResult<CiRunner> {
                Ok(CiRunner {
                    id: r.try_get(0)?,
                    name: r.try_get(1)?,
                    labels: words(&r.try_get::<String, _>(2)?),
                    created_ms: r.try_get(3)?,
                    last_seen: r.try_get(4)?,
                })
            }

            async fn next_number_tx(tx: &mut sqlx::Transaction<'_, $db>, repo: RepoId) -> StoreResult<i64> {
                let n: i64 = sqlx::query(
                    "SELECT COALESCE(MAX(n), 0) + 1 FROM (\
                       SELECT MAX(number) AS n FROM reviews WHERE repo_id = $1 \
                       UNION ALL SELECT MAX(number) AS n FROM issues WHERE repo_id = $1) AS t",
                )
                .bind(repo.0)
                .fetch_one(&mut **tx)
                .await?
                .try_get(0)?;
                Ok(n)
            }

            async fn number_taken_tx(tx: &mut sqlx::Transaction<'_, $db>, repo: RepoId, n: i64) -> StoreResult<bool> {
                let a: Option<i64> = sqlx::query_scalar("SELECT number FROM issues WHERE repo_id = $1 AND number = $2")
                    .bind(repo.0)
                    .bind(n)
                    .fetch_optional(&mut **tx)
                    .await?;
                let b: Option<i64> = sqlx::query_scalar("SELECT number FROM reviews WHERE repo_id = $1 AND number = $2")
                    .bind(repo.0)
                    .bind(n)
                    .fetch_optional(&mut **tx)
                    .await?;
                Ok(a.is_some() || b.is_some())
            }

            fn issue_filter(q: &IssueQuery, args: &mut Vec<Arg>) -> String {
                let mut w = String::from(" WHERE i.repo_id = $1");
                let push = |args: &mut Vec<Arg>, a: Arg| -> usize {
                    args.push(a);
                    args.len() + 1
                };
                if let Some(s) = &q.state {
                    let n = push(args, Arg::S(s.clone()));
                    w += &format!(" AND i.state = ${n}");
                }
                if let Some(l) = &q.label {
                    let n = push(args, Arg::S(l.clone()));
                    w += &format!(
                        " AND EXISTS (SELECT 1 FROM item_labels l WHERE l.repo_id = i.repo_id AND l.number = i.number AND l.label = ${n})"
                    );
                }
                if let Some(a) = &q.assignee {
                    let n = push(args, Arg::S(a.clone()));
                    w += &format!(
                        " AND EXISTS (SELECT 1 FROM item_people p WHERE p.repo_id = i.repo_id AND p.number = i.number \
                         AND p.kind = 'assignee' AND p.name = ${n})"
                    );
                }
                if let Some(a) = &q.author {
                    let n = push(args, Arg::S(a.clone()));
                    w += &format!(" AND i.author = ${n}");
                }
                if let Some(m) = q.milestone {
                    let n = push(args, Arg::I(m));
                    w += &format!(" AND i.milestone = ${n}");
                }
                if let Some(t) = q.text.as_ref().filter(|t| !t.trim().is_empty()) {
                    let pat = format!("%{}%", t.trim().to_lowercase().replace('\\', "").replace('%', "").replace('_', ""));
                    let n = push(args, Arg::S(pat));
                    w += &format!(" AND (LOWER(i.title) LIKE ${n} OR LOWER(i.body) LIKE ${n})");
                }
                w
            }
        }

        #[async_trait]
        impl CollabStore for $name {
            // ───── issues ─────

            async fn create_issue(&self, repo: RepoId, n: &NewIssue) -> StoreResult<Issue> {
                let _w = self.write_lock().await;
                let mut tx = self.pool.begin_with(Self::BEGIN_WRITE).await?;
                Self::lock_log(&mut tx, repo).await?;
                let number = match n.number {
                    Some(num) => {
                        if Self::number_taken_tx(&mut tx, repo, num).await? {
                            return Err(StoreError::Conflict(format!("#{num} already exists")));
                        }
                        num
                    }
                    None => Self::next_number_tx(&mut tx, repo).await?,
                };
                let now = now_ms();
                let created = n.created_ms.unwrap_or(now);
                let updated = n.updated_ms.unwrap_or(created);
                let state = if n.state.is_empty() { "open" } else { n.state.as_str() };
                let row = sqlx::query(&format!(
                    "INSERT INTO issues (repo_id, number, title, body, author, state, state_reason, milestone, locked, comments, \
                     created_at, updated_at, closed_at) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, 0, $10, $11, $12) RETURNING {ISSUE_COLS}"
                ))
                .bind(repo.0)
                .bind(number)
                .bind(&n.title)
                .bind(&n.body)
                .bind(&n.author)
                .bind(state)
                .bind(&n.state_reason)
                .bind(n.milestone)
                .bind(false)
                .bind(created)
                .bind(updated)
                .bind(n.closed_ms)
                .fetch_one(&mut *tx)
                .await?;
                let issue = Self::issue(&row)?;
                tx.commit().await?;
                Ok(issue)
            }

            async fn get_issue(&self, repo: RepoId, number: i64) -> StoreResult<Option<Issue>> {
                let row = sqlx::query(&format!("SELECT {ISSUE_COLS} FROM issues WHERE repo_id = $1 AND number = $2"))
                    .bind(repo.0)
                    .bind(number)
                    .fetch_optional(&self.pool)
                    .await?;
                row.map(|r| Self::issue(&r)).transpose()
            }

            async fn list_issues(&self, repo: RepoId, q: &IssueQuery) -> StoreResult<(Vec<Issue>, i64)> {
                let mut args = Vec::new();
                let filter = Self::issue_filter(q, &mut args);
                let order = match q.sort.as_deref() {
                    Some("created") => "i.number DESC",
                    Some("created-asc") => "i.number ASC",
                    Some("comments") => "i.comments DESC, i.number DESC",
                    _ => "i.updated_at DESC, i.number DESC",
                };
                let cols = ISSUE_COLS.split(", ").map(|c| format!("i.{c}")).collect::<Vec<_>>().join(", ");
                let sql = format!(
                    "SELECT {cols} FROM issues i{filter} ORDER BY {order} LIMIT {} OFFSET {}",
                    q.limit.clamp(1, 1000),
                    q.offset
                );
                let mut query = sqlx::query(&sql).bind(repo.0);
                for a in &args {
                    query = match a {
                        Arg::I(v) => query.bind(*v),
                        Arg::S(s) => query.bind(s.clone()),
                    };
                }
                let rows = query.fetch_all(&self.pool).await?;
                let items = rows.iter().map(Self::issue).collect::<StoreResult<Vec<_>>>()?;
                let csql = format!("SELECT COUNT(*) FROM issues i{filter}");
                let mut cq = sqlx::query(&csql).bind(repo.0);
                for a in &args {
                    cq = match a {
                        Arg::I(v) => cq.bind(*v),
                        Arg::S(s) => cq.bind(s.clone()),
                    };
                }
                let total: i64 = cq.fetch_one(&self.pool).await?.try_get(0)?;
                Ok((items, total))
            }

            async fn update_issue(&self, repo: RepoId, number: i64, u: &IssueUpdate) -> StoreResult<Option<Issue>> {
                let _w = self.write_lock().await;
                let mut tx = self.pool.begin_with(Self::BEGIN_WRITE).await?;
                let row = sqlx::query(&format!("SELECT {ISSUE_COLS} FROM issues WHERE repo_id = $1 AND number = $2"))
                    .bind(repo.0)
                    .bind(number)
                    .fetch_optional(&mut *tx)
                    .await?;
                let Some(row) = row else { return Ok(None) };
                let mut i = Self::issue(&row)?;
                let now = now_ms();
                if let Some(t) = &u.title {
                    i.title = t.clone();
                }
                if let Some(b) = &u.body {
                    i.body = b.clone();
                }
                if let Some(s) = &u.state {
                    if *s != i.state {
                        i.closed_ms = if s == "closed" { Some(now) } else { None };
                    }
                    i.state = s.clone();
                }
                if let Some(r) = &u.state_reason {
                    i.state_reason = r.clone();
                }
                if let Some(m) = u.milestone {
                    i.milestone = m;
                }
                if let Some(l) = u.locked {
                    i.locked = l;
                }
                i.updated_ms = now;
                sqlx::query(
                    "UPDATE issues SET title = $3, body = $4, state = $5, state_reason = $6, milestone = $7, locked = $8, \
                     updated_at = $9, closed_at = $10 WHERE repo_id = $1 AND number = $2",
                )
                .bind(repo.0)
                .bind(number)
                .bind(&i.title)
                .bind(&i.body)
                .bind(&i.state)
                .bind(&i.state_reason)
                .bind(i.milestone)
                .bind(i.locked)
                .bind(i.updated_ms)
                .bind(i.closed_ms)
                .execute(&mut *tx)
                .await?;
                tx.commit().await?;
                Ok(Some(i))
            }

            async fn issue_counts(&self, repo: RepoId) -> StoreResult<(i64, i64)> {
                let rows = sqlx::query("SELECT state, COUNT(*) FROM issues WHERE repo_id = $1 GROUP BY state")
                    .bind(repo.0)
                    .fetch_all(&self.pool)
                    .await?;
                let (mut open, mut closed) = (0, 0);
                for r in rows {
                    let s: String = r.try_get(0)?;
                    let n: i64 = r.try_get(1)?;
                    if s == "open" {
                        open = n
                    } else {
                        closed += n
                    }
                }
                Ok((open, closed))
            }

            async fn add_issue_event(
                &self,
                repo: RepoId,
                number: i64,
                kind: &str,
                author: &str,
                body: &str,
                created_ms: Option<i64>,
                external_id: Option<&str>,
            ) -> StoreResult<IssueEvent> {
                let _w = self.write_lock().await;
                let mut tx = self.pool.begin_with(Self::BEGIN_WRITE).await?;
                if let Some(x) = external_id {
                    let row = sqlx::query(
                        "SELECT seq, number, kind, author, body, created_at, updated_at FROM issue_events \
                         WHERE repo_id = $1 AND external_id = $2",
                    )
                    .bind(repo.0)
                    .bind(x)
                    .fetch_optional(&mut *tx)
                    .await?;
                    if let Some(r) = row {
                        return Self::issue_event(&r);
                    }
                }
                let ts = created_ms.unwrap_or_else(now_ms);
                let row = sqlx::query(
                    "INSERT INTO issue_events (repo_id, number, kind, author, body, created_at, updated_at, external_id) \
                     VALUES ($1, $2, $3, $4, $5, $6, $6, $7) RETURNING seq, number, kind, author, body, created_at, updated_at",
                )
                .bind(repo.0)
                .bind(number)
                .bind(kind)
                .bind(author)
                .bind(body)
                .bind(ts)
                .bind(external_id)
                .fetch_one(&mut *tx)
                .await?;
                let ev = Self::issue_event(&row)?;
                if kind == "comment" {
                    sqlx::query(
                        "UPDATE issues SET comments = comments + 1, updated_at = CASE WHEN updated_at < $3 THEN $3 ELSE updated_at END \
                         WHERE repo_id = $1 AND number = $2",
                    )
                    .bind(repo.0)
                    .bind(number)
                    .bind(ts)
                    .execute(&mut *tx)
                    .await?;
                }
                tx.commit().await?;
                Ok(ev)
            }

            async fn issue_events(&self, repo: RepoId, number: i64) -> StoreResult<Vec<IssueEvent>> {
                let rows = sqlx::query(
                    "SELECT seq, number, kind, author, body, created_at, updated_at FROM issue_events \
                     WHERE repo_id = $1 AND number = $2 ORDER BY created_at, seq",
                )
                .bind(repo.0)
                .bind(number)
                .fetch_all(&self.pool)
                .await?;
                rows.iter().map(Self::issue_event).collect()
            }

            async fn get_issue_event(&self, repo: RepoId, seq: i64) -> StoreResult<Option<IssueEvent>> {
                let row = sqlx::query(
                    "SELECT seq, number, kind, author, body, created_at, updated_at FROM issue_events WHERE repo_id = $1 AND seq = $2",
                )
                .bind(repo.0)
                .bind(seq)
                .fetch_optional(&self.pool)
                .await?;
                row.map(|r| Self::issue_event(&r)).transpose()
            }

            async fn update_issue_event(&self, repo: RepoId, seq: i64, body: &str) -> StoreResult<bool> {
                let n = sqlx::query("UPDATE issue_events SET body = $3, updated_at = $4 WHERE repo_id = $1 AND seq = $2")
                    .bind(repo.0)
                    .bind(seq)
                    .bind(body)
                    .bind(now_ms())
                    .execute(&self.pool)
                    .await?
                    .rows_affected();
                Ok(n > 0)
            }

            async fn delete_issue_event(&self, repo: RepoId, seq: i64) -> StoreResult<bool> {
                let _w = self.write_lock().await;
                let mut tx = self.pool.begin_with(Self::BEGIN_WRITE).await?;
                let row = sqlx::query("SELECT number, kind FROM issue_events WHERE repo_id = $1 AND seq = $2")
                    .bind(repo.0)
                    .bind(seq)
                    .fetch_optional(&mut *tx)
                    .await?;
                let Some(row) = row else { return Ok(false) };
                let number: i64 = row.try_get(0)?;
                let kind: String = row.try_get(1)?;
                sqlx::query("DELETE FROM issue_events WHERE repo_id = $1 AND seq = $2").bind(repo.0).bind(seq).execute(&mut *tx).await?;
                if kind == "comment" {
                    sqlx::query("UPDATE issues SET comments = comments - 1 WHERE repo_id = $1 AND number = $2 AND comments > 0")
                        .bind(repo.0)
                        .bind(number)
                        .execute(&mut *tx)
                        .await?;
                }
                tx.commit().await?;
                Ok(true)
            }

            async fn next_item_number(&self, repo: RepoId) -> StoreResult<i64> {
                let mut tx = self.pool.begin_with("BEGIN").await?;
                let n = Self::next_number_tx(&mut tx, repo).await?;
                tx.rollback().await?;
                Ok(n)
            }

            async fn item_kind(&self, repo: RepoId, number: i64) -> StoreResult<Option<&'static str>> {
                let a: Option<i64> = sqlx::query_scalar("SELECT number FROM issues WHERE repo_id = $1 AND number = $2")
                    .bind(repo.0)
                    .bind(number)
                    .fetch_optional(&self.pool)
                    .await?;
                if a.is_some() {
                    return Ok(Some("issue"));
                }
                let b: Option<i64> = sqlx::query_scalar("SELECT number FROM reviews WHERE repo_id = $1 AND number = $2")
                    .bind(repo.0)
                    .bind(number)
                    .fetch_optional(&self.pool)
                    .await?;
                Ok(b.map(|_| "review"))
            }

            // ───── reviews (extensions) ─────

            async fn import_review(&self, repo: RepoId, r: &ImportedReview) -> StoreResult<Review> {
                let _w = self.write_lock().await;
                let mut tx = self.pool.begin_with(Self::BEGIN_WRITE).await?;
                Self::lock_log(&mut tx, repo).await?;
                let existing: Option<i64> = sqlx::query_scalar("SELECT number FROM reviews WHERE repo_id = $1 AND number = $2")
                    .bind(repo.0)
                    .bind(r.number)
                    .fetch_optional(&mut *tx)
                    .await?;
                if existing.is_some() {
                    // Re-import (mirror sync): refresh the mutable fields.
                    sqlx::query(
                        "UPDATE reviews SET title = $3, body = $4, head = $5, target = $6, state = $7, landed = $8, \
                         updated_at = $9, draft = $10 WHERE repo_id = $1 AND number = $2",
                    )
                    .bind(repo.0)
                    .bind(r.number)
                    .bind(&r.title)
                    .bind(&r.body)
                    .bind(r.head.as_bytes().as_slice())
                    .bind(&r.target)
                    .bind(&r.state)
                    .bind(r.landed.map(|l| l.as_bytes().to_vec()))
                    .bind(r.updated_ms)
                    .bind(r.draft)
                    .execute(&mut *tx)
                    .await?;
                } else {
                    let taken: Option<i64> = sqlx::query_scalar("SELECT number FROM issues WHERE repo_id = $1 AND number = $2")
                        .bind(repo.0)
                        .bind(r.number)
                        .fetch_optional(&mut *tx)
                        .await?;
                    if taken.is_some() {
                        return Err(StoreError::Conflict(format!("#{} is an issue", r.number)));
                    }
                    sqlx::query(
                        "INSERT INTO reviews (repo_id, number, title, body, author, head, target, state, landed, created_at, \
                         updated_at, draft) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
                    )
                    .bind(repo.0)
                    .bind(r.number)
                    .bind(&r.title)
                    .bind(&r.body)
                    .bind(&r.author)
                    .bind(r.head.as_bytes().as_slice())
                    .bind(&r.target)
                    .bind(&r.state)
                    .bind(r.landed.map(|l| l.as_bytes().to_vec()))
                    .bind(r.created_ms)
                    .bind(r.updated_ms)
                    .bind(r.draft)
                    .execute(&mut *tx)
                    .await?;
                }
                let row = sqlx::query(
                    "SELECT number, title, body, author, head, target, state, landed, created_at, updated_at \
                     FROM reviews WHERE repo_id = $1 AND number = $2",
                )
                .bind(repo.0)
                .bind(r.number)
                .fetch_one(&mut *tx)
                .await?;
                let out = review_row(row)?;
                tx.commit().await?;
                Ok(out)
            }

            async fn import_review_event(
                &self,
                repo: RepoId,
                number: i64,
                kind: &str,
                author: &str,
                body: &str,
                path: Option<&[u8]>,
                line: Option<i64>,
                ts_ms: i64,
                external_id: &str,
            ) -> StoreResult<bool> {
                let r = sqlx::query(
                    "INSERT INTO review_events (repo_id, number, kind, author, body, cs_id, path, line, ts, external_id) \
                     VALUES ($1, $2, $3, $4, $5, NULL, $6, $7, $8, $9) ON CONFLICT DO NOTHING",
                )
                .bind(repo.0)
                .bind(number)
                .bind(kind)
                .bind(author)
                .bind(body)
                .bind(path)
                .bind(line)
                .bind(ts_ms)
                .bind(external_id)
                .execute(&self.pool)
                .await?;
                Ok(r.rows_affected() > 0)
            }

            async fn set_review_draft(&self, repo: RepoId, number: i64, draft: bool) -> StoreResult<()> {
                sqlx::query("UPDATE reviews SET draft = $3, updated_at = $4 WHERE repo_id = $1 AND number = $2")
                    .bind(repo.0)
                    .bind(number)
                    .bind(draft)
                    .bind(now_ms())
                    .execute(&self.pool)
                    .await?;
                Ok(())
            }

            async fn review_flags(&self, repo: RepoId, number: i64) -> StoreResult<(bool, i64)> {
                let row = sqlx::query("SELECT draft, source_repo FROM reviews WHERE repo_id = $1 AND number = $2")
                    .bind(repo.0)
                    .bind(number)
                    .fetch_optional(&self.pool)
                    .await?;
                match row {
                    Some(r) => Ok((r.try_get(0)?, r.try_get(1)?)),
                    None => Ok((false, 0)),
                }
            }

            async fn set_review_source(&self, repo: RepoId, number: i64, source: RepoId) -> StoreResult<()> {
                sqlx::query("UPDATE reviews SET source_repo = $3 WHERE repo_id = $1 AND number = $2")
                    .bind(repo.0)
                    .bind(number)
                    .bind(source.0)
                    .execute(&self.pool)
                    .await?;
                Ok(())
            }

            async fn review_count(&self, repo: RepoId, state: &str) -> StoreResult<i64> {
                Ok(sqlx::query_scalar("SELECT COUNT(*) FROM reviews WHERE repo_id = $1 AND state = $2")
                    .bind(repo.0)
                    .bind(state)
                    .fetch_one(&self.pool)
                    .await?)
            }

            // ───── labels, people, milestones ─────

            async fn put_label(&self, repo: RepoId, l: &Label) -> StoreResult<()> {
                sqlx::query(
                    "INSERT INTO labels (repo_id, name, color, description) VALUES ($1, $2, $3, $4) \
                     ON CONFLICT (repo_id, name) DO UPDATE SET color = excluded.color, description = excluded.description",
                )
                .bind(repo.0)
                .bind(&l.name)
                .bind(&l.color)
                .bind(&l.description)
                .execute(&self.pool)
                .await?;
                Ok(())
            }

            async fn rename_label(&self, repo: RepoId, old: &str, l: &Label) -> StoreResult<bool> {
                let _w = self.write_lock().await;
                let mut tx = self.pool.begin_with(Self::BEGIN_WRITE).await?;
                let n = sqlx::query("UPDATE labels SET name = $3, color = $4, description = $5 WHERE repo_id = $1 AND name = $2")
                    .bind(repo.0)
                    .bind(old)
                    .bind(&l.name)
                    .bind(&l.color)
                    .bind(&l.description)
                    .execute(&mut *tx)
                    .await?
                    .rows_affected();
                if n > 0 && old != l.name {
                    sqlx::query("UPDATE item_labels SET label = $3 WHERE repo_id = $1 AND label = $2")
                        .bind(repo.0)
                        .bind(old)
                        .bind(&l.name)
                        .execute(&mut *tx)
                        .await?;
                }
                tx.commit().await?;
                Ok(n > 0)
            }

            async fn delete_label(&self, repo: RepoId, name: &str) -> StoreResult<bool> {
                let _w = self.write_lock().await;
                let mut tx = self.pool.begin_with(Self::BEGIN_WRITE).await?;
                let n = sqlx::query("DELETE FROM labels WHERE repo_id = $1 AND name = $2")
                    .bind(repo.0)
                    .bind(name)
                    .execute(&mut *tx)
                    .await?
                    .rows_affected();
                sqlx::query("DELETE FROM item_labels WHERE repo_id = $1 AND label = $2").bind(repo.0).bind(name).execute(&mut *tx).await?;
                tx.commit().await?;
                Ok(n > 0)
            }

            async fn list_labels(&self, repo: RepoId) -> StoreResult<Vec<Label>> {
                let rows = sqlx::query("SELECT name, color, description FROM labels WHERE repo_id = $1 ORDER BY LOWER(name)")
                    .bind(repo.0)
                    .fetch_all(&self.pool)
                    .await?;
                rows.iter()
                    .map(|r| Ok(Label { name: r.try_get(0)?, color: r.try_get(1)?, description: r.try_get(2)? }))
                    .collect()
            }

            async fn set_item_labels(&self, repo: RepoId, number: i64, labels: &[String]) -> StoreResult<()> {
                let _w = self.write_lock().await;
                let mut tx = self.pool.begin_with(Self::BEGIN_WRITE).await?;
                sqlx::query("DELETE FROM item_labels WHERE repo_id = $1 AND number = $2")
                    .bind(repo.0)
                    .bind(number)
                    .execute(&mut *tx)
                    .await?;
                for l in labels {
                    sqlx::query("INSERT INTO item_labels (repo_id, number, label) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING")
                        .bind(repo.0)
                        .bind(number)
                        .bind(l)
                        .execute(&mut *tx)
                        .await?;
                }
                tx.commit().await?;
                Ok(())
            }

            async fn item_labels(&self, repo: RepoId, numbers: &[i64]) -> StoreResult<Vec<(i64, String)>> {
                let mut out = Vec::new();
                for chunk in numbers.chunks(IN_CHUNK) {
                    let sql = format!(
                        "SELECT number, label FROM item_labels WHERE repo_id = $1 AND number IN ({}) ORDER BY number, label",
                        placeholders(2, chunk.len())
                    );
                    let mut q = sqlx::query(&sql).bind(repo.0);
                    for n in chunk {
                        q = q.bind(*n);
                    }
                    for r in q.fetch_all(&self.pool).await? {
                        out.push((r.try_get(0)?, r.try_get(1)?));
                    }
                }
                Ok(out)
            }

            async fn set_item_people(&self, repo: RepoId, number: i64, kind: &str, names: &[String]) -> StoreResult<()> {
                let _w = self.write_lock().await;
                let mut tx = self.pool.begin_with(Self::BEGIN_WRITE).await?;
                sqlx::query("DELETE FROM item_people WHERE repo_id = $1 AND number = $2 AND kind = $3")
                    .bind(repo.0)
                    .bind(number)
                    .bind(kind)
                    .execute(&mut *tx)
                    .await?;
                for n in names {
                    sqlx::query("INSERT INTO item_people (repo_id, number, kind, name) VALUES ($1, $2, $3, $4) ON CONFLICT DO NOTHING")
                        .bind(repo.0)
                        .bind(number)
                        .bind(kind)
                        .bind(n)
                        .execute(&mut *tx)
                        .await?;
                }
                tx.commit().await?;
                Ok(())
            }

            async fn item_people(&self, repo: RepoId, numbers: &[i64], kind: &str) -> StoreResult<Vec<(i64, String)>> {
                let mut out = Vec::new();
                for chunk in numbers.chunks(IN_CHUNK) {
                    let sql = format!(
                        "SELECT number, name FROM item_people WHERE repo_id = $1 AND kind = $2 AND number IN ({}) ORDER BY number, name",
                        placeholders(3, chunk.len())
                    );
                    let mut q = sqlx::query(&sql).bind(repo.0).bind(kind);
                    for n in chunk {
                        q = q.bind(*n);
                    }
                    for r in q.fetch_all(&self.pool).await? {
                        out.push((r.try_get(0)?, r.try_get(1)?));
                    }
                }
                Ok(out)
            }

            async fn create_milestone(
                &self,
                repo: RepoId,
                number: Option<i64>,
                title: &str,
                description: &str,
                due_ms: Option<i64>,
            ) -> StoreResult<Milestone> {
                let _w = self.write_lock().await;
                let mut tx = self.pool.begin_with(Self::BEGIN_WRITE).await?;
                let number = match number {
                    Some(n) => n,
                    None => sqlx::query_scalar("SELECT COALESCE(MAX(number), 0) + 1 FROM milestones WHERE repo_id = $1")
                        .bind(repo.0)
                        .fetch_one(&mut *tx)
                        .await?,
                };
                let now = now_ms();
                let r = sqlx::query(
                    "INSERT INTO milestones (repo_id, number, title, description, state, due_at, created_at) \
                     VALUES ($1, $2, $3, $4, 'open', $5, $6) ON CONFLICT (repo_id, number) DO NOTHING",
                )
                .bind(repo.0)
                .bind(number)
                .bind(title)
                .bind(description)
                .bind(due_ms)
                .bind(now)
                .execute(&mut *tx)
                .await?;
                if r.rows_affected() == 0 {
                    return Err(StoreError::Conflict(format!("milestone {number} already exists")));
                }
                tx.commit().await?;
                Ok(Milestone {
                    number,
                    title: title.into(),
                    description: description.into(),
                    state: "open".into(),
                    due_ms,
                    created_ms: now,
                    closed_ms: None,
                    open_items: 0,
                    closed_items: 0,
                })
            }

            async fn update_milestone(
                &self,
                repo: RepoId,
                number: i64,
                title: &str,
                description: &str,
                state: &str,
                due_ms: Option<i64>,
            ) -> StoreResult<bool> {
                let closed = if state == "closed" { Some(now_ms()) } else { None };
                let n = sqlx::query(
                    "UPDATE milestones SET title = $3, description = $4, state = $5, due_at = $6, \
                     closed_at = CASE WHEN $5 = 'closed' THEN COALESCE(closed_at, $7) ELSE NULL END \
                     WHERE repo_id = $1 AND number = $2",
                )
                .bind(repo.0)
                .bind(number)
                .bind(title)
                .bind(description)
                .bind(state)
                .bind(due_ms)
                .bind(closed)
                .execute(&self.pool)
                .await?
                .rows_affected();
                Ok(n > 0)
            }

            async fn delete_milestone(&self, repo: RepoId, number: i64) -> StoreResult<bool> {
                let _w = self.write_lock().await;
                let mut tx = self.pool.begin_with(Self::BEGIN_WRITE).await?;
                let n = sqlx::query("DELETE FROM milestones WHERE repo_id = $1 AND number = $2")
                    .bind(repo.0)
                    .bind(number)
                    .execute(&mut *tx)
                    .await?
                    .rows_affected();
                sqlx::query("UPDATE issues SET milestone = NULL WHERE repo_id = $1 AND milestone = $2")
                    .bind(repo.0)
                    .bind(number)
                    .execute(&mut *tx)
                    .await?;
                sqlx::query("UPDATE reviews SET milestone = NULL WHERE repo_id = $1 AND milestone = $2")
                    .bind(repo.0)
                    .bind(number)
                    .execute(&mut *tx)
                    .await?;
                tx.commit().await?;
                Ok(n > 0)
            }

            async fn list_milestones(&self, repo: RepoId) -> StoreResult<Vec<Milestone>> {
                let rows = sqlx::query(
                    "SELECT m.number, m.title, m.description, m.state, m.due_at, m.created_at, m.closed_at, \
                       (SELECT COUNT(*) FROM issues i WHERE i.repo_id = m.repo_id AND i.milestone = m.number AND i.state = 'open'), \
                       (SELECT COUNT(*) FROM issues i WHERE i.repo_id = m.repo_id AND i.milestone = m.number AND i.state <> 'open') \
                     FROM milestones m WHERE m.repo_id = $1 ORDER BY m.state DESC, m.due_at, m.number",
                )
                .bind(repo.0)
                .fetch_all(&self.pool)
                .await?;
                rows.iter().map(Self::milestone).collect()
            }

            // ───── releases ─────

            async fn create_release(&self, repo: RepoId, r: &Release) -> StoreResult<Release> {
                let row = sqlx::query(
                    "INSERT INTO releases (repo_id, tag, name, body, draft, prerelease, author, target, created_at, published_at) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) ON CONFLICT (repo_id, tag) DO NOTHING RETURNING release_id",
                )
                .bind(repo.0)
                .bind(&r.tag)
                .bind(&r.name)
                .bind(&r.body)
                .bind(r.draft)
                .bind(r.prerelease)
                .bind(&r.author)
                .bind(r.target.map(|t| t.as_bytes().to_vec()))
                .bind(r.created_ms)
                .bind(r.published_ms)
                .fetch_optional(&self.pool)
                .await?;
                let Some(row) = row else { return Err(StoreError::Conflict(format!("a release for {} already exists", r.tag))) };
                Ok(Release { id: row.try_get(0)?, ..r.clone() })
            }

            async fn update_release(&self, repo: RepoId, r: &Release) -> StoreResult<bool> {
                let n = sqlx::query(
                    "UPDATE releases SET tag = $3, name = $4, body = $5, draft = $6, prerelease = $7, target = $8, published_at = $9 \
                     WHERE repo_id = $1 AND release_id = $2",
                )
                .bind(repo.0)
                .bind(r.id)
                .bind(&r.tag)
                .bind(&r.name)
                .bind(&r.body)
                .bind(r.draft)
                .bind(r.prerelease)
                .bind(r.target.map(|t| t.as_bytes().to_vec()))
                .bind(r.published_ms)
                .execute(&self.pool)
                .await?
                .rows_affected();
                Ok(n > 0)
            }

            async fn delete_release(&self, repo: RepoId, id: i64) -> StoreResult<bool> {
                let _w = self.write_lock().await;
                let mut tx = self.pool.begin_with(Self::BEGIN_WRITE).await?;
                let n = sqlx::query("DELETE FROM releases WHERE repo_id = $1 AND release_id = $2")
                    .bind(repo.0)
                    .bind(id)
                    .execute(&mut *tx)
                    .await?
                    .rows_affected();
                if n > 0 {
                    sqlx::query("DELETE FROM release_assets WHERE release_id = $1").bind(id).execute(&mut *tx).await?;
                }
                tx.commit().await?;
                Ok(n > 0)
            }

            async fn get_release(&self, repo: RepoId, id: i64) -> StoreResult<Option<Release>> {
                let row = sqlx::query(
                    "SELECT release_id, tag, name, body, draft, prerelease, author, target, created_at, published_at \
                     FROM releases WHERE repo_id = $1 AND release_id = $2",
                )
                .bind(repo.0)
                .bind(id)
                .fetch_optional(&self.pool)
                .await?;
                row.map(|r| Self::release(&r)).transpose()
            }

            async fn release_by_tag(&self, repo: RepoId, tag: &str) -> StoreResult<Option<Release>> {
                let row = sqlx::query(
                    "SELECT release_id, tag, name, body, draft, prerelease, author, target, created_at, published_at \
                     FROM releases WHERE repo_id = $1 AND tag = $2",
                )
                .bind(repo.0)
                .bind(tag)
                .fetch_optional(&self.pool)
                .await?;
                row.map(|r| Self::release(&r)).transpose()
            }

            async fn list_releases(&self, repo: RepoId, limit: u32) -> StoreResult<Vec<Release>> {
                let rows = sqlx::query(
                    "SELECT release_id, tag, name, body, draft, prerelease, author, target, created_at, published_at \
                     FROM releases WHERE repo_id = $1 ORDER BY COALESCE(published_at, created_at) DESC, release_id DESC LIMIT $2",
                )
                .bind(repo.0)
                .bind(limit as i64)
                .fetch_all(&self.pool)
                .await?;
                rows.iter().map(Self::release).collect()
            }

            async fn add_release_asset(&self, a: &ReleaseAsset) -> StoreResult<ReleaseAsset> {
                let row = sqlx::query(
                    "INSERT INTO release_assets (release_id, name, content_id, size, content_type, downloads, created_at) \
                     VALUES ($1, $2, $3, $4, $5, 0, $6) ON CONFLICT (release_id, name) DO NOTHING RETURNING asset_id",
                )
                .bind(a.release_id)
                .bind(&a.name)
                .bind(a.content.as_bytes().as_slice())
                .bind(a.size)
                .bind(&a.content_type)
                .bind(a.created_ms)
                .fetch_optional(&self.pool)
                .await?;
                let Some(row) = row else { return Err(StoreError::Conflict(format!("an asset named {} exists", a.name))) };
                Ok(ReleaseAsset { id: row.try_get(0)?, downloads: 0, ..a.clone() })
            }

            async fn release_assets(&self, release_id: i64) -> StoreResult<Vec<ReleaseAsset>> {
                let rows = sqlx::query(
                    "SELECT asset_id, release_id, name, content_id, size, content_type, downloads, created_at \
                     FROM release_assets WHERE release_id = $1 ORDER BY name",
                )
                .bind(release_id)
                .fetch_all(&self.pool)
                .await?;
                rows.iter().map(Self::asset).collect()
            }

            async fn delete_release_asset(&self, release_id: i64, id: i64) -> StoreResult<bool> {
                let n = sqlx::query("DELETE FROM release_assets WHERE release_id = $1 AND asset_id = $2")
                    .bind(release_id)
                    .bind(id)
                    .execute(&self.pool)
                    .await?
                    .rows_affected();
                Ok(n > 0)
            }

            async fn count_asset_download(&self, id: i64) -> StoreResult<()> {
                sqlx::query("UPDATE release_assets SET downloads = downloads + 1 WHERE asset_id = $1").bind(id).execute(&self.pool).await?;
                Ok(())
            }

            // ───── commit statuses ─────

            async fn put_status(&self, repo: RepoId, cs: &ChangesetId, s: &CommitStatus) -> StoreResult<()> {
                let now = now_ms();
                sqlx::query(
                    "INSERT INTO statuses (repo_id, cs_id, context, state, description, target_url, creator, created_at, updated_at) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $8) ON CONFLICT (repo_id, cs_id, context) DO UPDATE SET \
                     state = excluded.state, description = excluded.description, target_url = excluded.target_url, \
                     creator = excluded.creator, updated_at = excluded.updated_at",
                )
                .bind(repo.0)
                .bind(cs.as_bytes().as_slice())
                .bind(&s.context)
                .bind(&s.state)
                .bind(&s.description)
                .bind(&s.target_url)
                .bind(&s.creator)
                .bind(now)
                .execute(&self.pool)
                .await?;
                Ok(())
            }

            async fn statuses(&self, repo: RepoId, cs: &ChangesetId) -> StoreResult<Vec<CommitStatus>> {
                let rows = sqlx::query(
                    "SELECT context, state, description, target_url, creator, created_at, updated_at FROM statuses \
                     WHERE repo_id = $1 AND cs_id = $2 ORDER BY context",
                )
                .bind(repo.0)
                .bind(cs.as_bytes().as_slice())
                .fetch_all(&self.pool)
                .await?;
                rows.iter().map(|r| Self::status(r, 0)).collect()
            }

            async fn statuses_many(&self, repo: RepoId, ids: &[ChangesetId]) -> StoreResult<Vec<(ChangesetId, CommitStatus)>> {
                let mut out = Vec::new();
                for chunk in ids.chunks(IN_CHUNK) {
                    let sql = format!(
                        "SELECT cs_id, context, state, description, target_url, creator, created_at, updated_at FROM statuses \
                         WHERE repo_id = $1 AND cs_id IN ({}) ORDER BY context",
                        placeholders(2, chunk.len())
                    );
                    let mut q = sqlx::query(&sql).bind(repo.0);
                    for id in chunk {
                        q = q.bind(id.as_bytes().to_vec());
                    }
                    for r in q.fetch_all(&self.pool).await? {
                        out.push((cs_id(&r.try_get::<Vec<u8>, _>(0)?)?, Self::status(&r, 1)?));
                    }
                }
                Ok(out)
            }

            // ───── webhooks ─────

            async fn create_webhook(&self, h: &Webhook) -> StoreResult<Webhook> {
                let row = sqlx::query(
                    "INSERT INTO webhooks (scope_kind, scope_id, url, secret, events, active, created_at) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING hook_id",
                )
                .bind(&h.scope_kind)
                .bind(h.scope_id)
                .bind(&h.url)
                .bind(&h.secret)
                .bind(h.events.join(" "))
                .bind(h.active)
                .bind(h.created_ms)
                .fetch_one(&self.pool)
                .await?;
                Ok(Webhook { id: row.try_get(0)?, ..h.clone() })
            }

            async fn update_webhook(&self, h: &Webhook) -> StoreResult<bool> {
                let n = sqlx::query("UPDATE webhooks SET url = $2, secret = $3, events = $4, active = $5 WHERE hook_id = $1")
                    .bind(h.id)
                    .bind(&h.url)
                    .bind(&h.secret)
                    .bind(h.events.join(" "))
                    .bind(h.active)
                    .execute(&self.pool)
                    .await?
                    .rows_affected();
                Ok(n > 0)
            }

            async fn delete_webhook(&self, id: i64) -> StoreResult<bool> {
                let _w = self.write_lock().await;
                let mut tx = self.pool.begin_with(Self::BEGIN_WRITE).await?;
                let n = sqlx::query("DELETE FROM webhooks WHERE hook_id = $1").bind(id).execute(&mut *tx).await?.rows_affected();
                sqlx::query("DELETE FROM webhook_deliveries WHERE hook_id = $1").bind(id).execute(&mut *tx).await?;
                tx.commit().await?;
                Ok(n > 0)
            }

            async fn get_webhook(&self, id: i64) -> StoreResult<Option<Webhook>> {
                let row = sqlx::query(
                    "SELECT hook_id, scope_kind, scope_id, url, secret, events, active, created_at FROM webhooks WHERE hook_id = $1",
                )
                .bind(id)
                .fetch_optional(&self.pool)
                .await?;
                row.map(|r| Self::webhook(&r)).transpose()
            }

            async fn list_webhooks(&self, scope_kind: &str, scope_id: i64) -> StoreResult<Vec<Webhook>> {
                let rows = sqlx::query(
                    "SELECT hook_id, scope_kind, scope_id, url, secret, events, active, created_at FROM webhooks \
                     WHERE scope_kind = $1 AND scope_id = $2 ORDER BY hook_id",
                )
                .bind(scope_kind)
                .bind(scope_id)
                .fetch_all(&self.pool)
                .await?;
                rows.iter().map(Self::webhook).collect()
            }

            async fn enqueue_delivery(&self, hook_id: i64, event: &str, guid: &str, payload: &str) -> StoreResult<i64> {
                let now = now_ms();
                let row = sqlx::query(
                    "INSERT INTO webhook_deliveries (hook_id, event, guid, payload, state, attempts, next_at, created_at) \
                     VALUES ($1, $2, $3, $4, 'pending', 0, $5, $5) RETURNING delivery_id",
                )
                .bind(hook_id)
                .bind(event)
                .bind(guid)
                .bind(payload)
                .bind(now)
                .fetch_one(&self.pool)
                .await?;
                Ok(row.try_get(0)?)
            }

            async fn claim_deliveries(&self, now: i64, lease_until: i64, limit: u32) -> StoreResult<Vec<Delivery>> {
                let rows = sqlx::query(
                    "SELECT delivery_id, hook_id, event, guid, payload, state, attempts, next_at, status_code, response, duration_ms, \
                     created_at FROM webhook_deliveries WHERE state = 'pending' AND next_at <= $1 ORDER BY next_at LIMIT $2",
                )
                .bind(now)
                .bind(limit as i64)
                .fetch_all(&self.pool)
                .await?;
                let mut out = Vec::new();
                for r in &rows {
                    let mut d = Self::delivery(r)?;
                    let n = sqlx::query(
                        "UPDATE webhook_deliveries SET next_at = $3, attempts = attempts + 1 \
                         WHERE delivery_id = $1 AND next_at = $2 AND state = 'pending'",
                    )
                    .bind(d.id)
                    .bind(d.next_ms)
                    .bind(lease_until)
                    .execute(&self.pool)
                    .await?
                    .rows_affected();
                    if n == 1 {
                        d.attempts += 1;
                        d.next_ms = lease_until;
                        out.push(d);
                    }
                }
                Ok(out)
            }

            async fn finish_delivery(
                &self,
                id: i64,
                state: &str,
                next_ms: i64,
                status_code: i64,
                response: &str,
                duration_ms: i64,
            ) -> StoreResult<()> {
                sqlx::query(
                    "UPDATE webhook_deliveries SET state = $2, next_at = $3, status_code = $4, response = $5, duration_ms = $6 \
                     WHERE delivery_id = $1",
                )
                .bind(id)
                .bind(state)
                .bind(next_ms)
                .bind(status_code)
                .bind(response)
                .bind(duration_ms)
                .execute(&self.pool)
                .await?;
                Ok(())
            }

            async fn list_deliveries(&self, hook_id: i64, limit: u32) -> StoreResult<Vec<Delivery>> {
                let rows = sqlx::query(
                    "SELECT delivery_id, hook_id, event, guid, payload, state, attempts, next_at, status_code, response, duration_ms, \
                     created_at FROM webhook_deliveries WHERE hook_id = $1 ORDER BY delivery_id DESC LIMIT $2",
                )
                .bind(hook_id)
                .bind(limit as i64)
                .fetch_all(&self.pool)
                .await?;
                rows.iter().map(Self::delivery).collect()
            }

            async fn get_delivery(&self, id: i64) -> StoreResult<Option<Delivery>> {
                let row = sqlx::query(
                    "SELECT delivery_id, hook_id, event, guid, payload, state, attempts, next_at, status_code, response, duration_ms, \
                     created_at FROM webhook_deliveries WHERE delivery_id = $1",
                )
                .bind(id)
                .fetch_optional(&self.pool)
                .await?;
                row.map(|r| Self::delivery(&r)).transpose()
            }

            async fn prune_deliveries(&self, before_ms: i64) -> StoreResult<u64> {
                Ok(sqlx::query("DELETE FROM webhook_deliveries WHERE state <> 'pending' AND created_at < $1")
                    .bind(before_ms)
                    .execute(&self.pool)
                    .await?
                    .rows_affected())
            }

            // ───── SSH keys ─────

            async fn add_ssh_key(&self, user_id: i64, title: &str, fingerprint: &str, public_key: &str) -> StoreResult<SshKey> {
                let now = now_ms();
                let row = sqlx::query(
                    "INSERT INTO ssh_keys (user_id, title, fingerprint, public_key, created_at) VALUES ($1, $2, $3, $4, $5) \
                     ON CONFLICT (fingerprint) DO NOTHING RETURNING key_id",
                )
                .bind(user_id)
                .bind(title)
                .bind(fingerprint)
                .bind(public_key)
                .bind(now)
                .fetch_optional(&self.pool)
                .await?;
                let Some(row) = row else { return Err(StoreError::Conflict("that key is already in use".into())) };
                Ok(SshKey {
                    id: row.try_get(0)?,
                    user_id,
                    title: title.into(),
                    fingerprint: fingerprint.into(),
                    public_key: public_key.into(),
                    created_ms: now,
                    last_used: None,
                })
            }

            async fn list_ssh_keys(&self, user_id: i64) -> StoreResult<Vec<SshKey>> {
                let rows = sqlx::query(
                    "SELECT key_id, user_id, title, fingerprint, public_key, created_at, last_used FROM ssh_keys \
                     WHERE user_id = $1 ORDER BY key_id",
                )
                .bind(user_id)
                .fetch_all(&self.pool)
                .await?;
                rows.iter().map(Self::ssh_key).collect()
            }

            async fn delete_ssh_key(&self, user_id: i64, id: i64) -> StoreResult<bool> {
                Ok(sqlx::query("DELETE FROM ssh_keys WHERE user_id = $1 AND key_id = $2")
                    .bind(user_id)
                    .bind(id)
                    .execute(&self.pool)
                    .await?
                    .rows_affected()
                    > 0)
            }

            async fn ssh_key_by_fingerprint(&self, fingerprint: &str) -> StoreResult<Option<SshKey>> {
                let row = sqlx::query(
                    "SELECT key_id, user_id, title, fingerprint, public_key, created_at, last_used FROM ssh_keys WHERE fingerprint = $1",
                )
                .bind(fingerprint)
                .fetch_optional(&self.pool)
                .await?;
                row.map(|r| Self::ssh_key(&r)).transpose()
            }

            async fn touch_ssh_key(&self, id: i64, now: i64) -> StoreResult<()> {
                sqlx::query("UPDATE ssh_keys SET last_used = $2 WHERE key_id = $1").bind(id).bind(now).execute(&self.pool).await?;
                Ok(())
            }

            // ───── notifications ─────

            async fn notify(&self, user_id: i64, repo: RepoId, number: i64, kind: &str, reason: &str, title: &str) -> StoreResult<()> {
                sqlx::query(
                    "INSERT INTO notifications (user_id, repo_id, number, kind, reason, title, unread, updated_at) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8) ON CONFLICT (user_id, repo_id, number, kind) DO UPDATE SET \
                     reason = excluded.reason, title = excluded.title, unread = excluded.unread, updated_at = excluded.updated_at",
                )
                .bind(user_id)
                .bind(repo.0)
                .bind(number)
                .bind(kind)
                .bind(reason)
                .bind(title)
                .bind(true)
                .bind(now_ms())
                .execute(&self.pool)
                .await?;
                Ok(())
            }

            async fn notifications(&self, user_id: i64, unread_only: bool, limit: u32) -> StoreResult<Vec<Notification>> {
                let sql = if unread_only {
                    "SELECT notification_id, repo_id, number, kind, reason, title, unread, updated_at FROM notifications \
                     WHERE user_id = $1 AND unread = $3 ORDER BY updated_at DESC LIMIT $2"
                } else {
                    "SELECT notification_id, repo_id, number, kind, reason, title, unread, updated_at FROM notifications \
                     WHERE user_id = $1 AND (unread = $3 OR unread <> $3) ORDER BY updated_at DESC LIMIT $2"
                };
                let rows = sqlx::query(sql).bind(user_id).bind(limit as i64).bind(true).fetch_all(&self.pool).await?;
                rows.iter()
                    .map(|r| {
                        Ok(Notification {
                            id: r.try_get(0)?,
                            repo_id: RepoId(r.try_get(1)?),
                            number: r.try_get(2)?,
                            kind: r.try_get(3)?,
                            reason: r.try_get(4)?,
                            title: r.try_get(5)?,
                            unread: r.try_get(6)?,
                            updated_ms: r.try_get(7)?,
                        })
                    })
                    .collect()
            }

            async fn unread_count(&self, user_id: i64) -> StoreResult<i64> {
                Ok(sqlx::query_scalar("SELECT COUNT(*) FROM notifications WHERE user_id = $1 AND unread = $2")
                    .bind(user_id)
                    .bind(true)
                    .fetch_one(&self.pool)
                    .await?)
            }

            async fn mark_read(&self, user_id: i64, id: Option<i64>) -> StoreResult<u64> {
                let r = match id {
                    Some(id) => {
                        sqlx::query("UPDATE notifications SET unread = $3 WHERE user_id = $1 AND notification_id = $2")
                            .bind(user_id)
                            .bind(id)
                            .bind(false)
                            .execute(&self.pool)
                            .await?
                    }
                    None => {
                        sqlx::query("UPDATE notifications SET unread = $2 WHERE user_id = $1 AND unread = $3")
                            .bind(user_id)
                            .bind(false)
                            .bind(true)
                            .execute(&self.pool)
                            .await?
                    }
                };
                Ok(r.rows_affected())
            }

            async fn set_subscription(&self, user_id: i64, repo: RepoId, number: i64, state: Option<&str>) -> StoreResult<()> {
                match state {
                    Some(s) => {
                        sqlx::query(
                            "INSERT INTO subscriptions (user_id, repo_id, number, state) VALUES ($1, $2, $3, $4) \
                             ON CONFLICT (user_id, repo_id, number) DO UPDATE SET state = excluded.state",
                        )
                        .bind(user_id)
                        .bind(repo.0)
                        .bind(number)
                        .bind(s)
                        .execute(&self.pool)
                        .await?;
                    }
                    None => {
                        sqlx::query("DELETE FROM subscriptions WHERE user_id = $1 AND repo_id = $2 AND number = $3")
                            .bind(user_id)
                            .bind(repo.0)
                            .bind(number)
                            .execute(&self.pool)
                            .await?;
                    }
                }
                Ok(())
            }

            async fn subscription(&self, user_id: i64, repo: RepoId, number: i64) -> StoreResult<Option<String>> {
                Ok(sqlx::query_scalar("SELECT state FROM subscriptions WHERE user_id = $1 AND repo_id = $2 AND number = $3")
                    .bind(user_id)
                    .bind(repo.0)
                    .bind(number)
                    .fetch_optional(&self.pool)
                    .await?)
            }

            async fn subscribers(&self, repo: RepoId, number: i64) -> StoreResult<Vec<(i64, i64, String)>> {
                let rows = sqlx::query(
                    "SELECT user_id, number, state FROM subscriptions WHERE repo_id = $1 AND (number = 0 OR number = $2)",
                )
                .bind(repo.0)
                .bind(number)
                .fetch_all(&self.pool)
                .await?;
                rows.iter().map(|r| Ok((r.try_get(0)?, r.try_get(1)?, r.try_get(2)?))).collect()
            }

            // ───── accounts (extensions) ─────

            async fn user_kind(&self, user_id: i64) -> StoreResult<String> {
                Ok(sqlx::query_scalar::<_, String>("SELECT kind FROM users WHERE user_id = $1")
                    .bind(user_id)
                    .fetch_optional(&self.pool)
                    .await?
                    .unwrap_or_else(|| "user".into()))
            }

            async fn set_user_kind(&self, user_id: i64, kind: &str) -> StoreResult<()> {
                sqlx::query("UPDATE users SET kind = $2 WHERE user_id = $1").bind(user_id).bind(kind).execute(&self.pool).await?;
                Ok(())
            }

            async fn link_identity(&self, provider: &str, subject: &str, user_id: i64, login: &str) -> StoreResult<()> {
                let r = sqlx::query(
                    "INSERT INTO identities (provider, subject, user_id, login, created_at) VALUES ($1, $2, $3, $4, $5) \
                     ON CONFLICT (provider, subject) DO UPDATE SET login = excluded.login WHERE identities.user_id = excluded.user_id",
                )
                .bind(provider)
                .bind(subject)
                .bind(user_id)
                .bind(login)
                .bind(now_ms())
                .execute(&self.pool)
                .await?;
                if r.rows_affected() == 0 {
                    return Err(StoreError::Conflict(format!("that {provider} account is linked to another user")));
                }
                Ok(())
            }

            async fn unlink_identity(&self, user_id: i64, provider: &str) -> StoreResult<bool> {
                Ok(sqlx::query("DELETE FROM identities WHERE user_id = $1 AND provider = $2")
                    .bind(user_id)
                    .bind(provider)
                    .execute(&self.pool)
                    .await?
                    .rows_affected()
                    > 0)
            }

            async fn identity(&self, provider: &str, subject: &str) -> StoreResult<Option<Identity>> {
                let row = sqlx::query(
                    "SELECT provider, subject, user_id, login, created_at FROM identities WHERE provider = $1 AND subject = $2",
                )
                .bind(provider)
                .bind(subject)
                .fetch_optional(&self.pool)
                .await?;
                row.map(|r| Self::identity_row(&r)).transpose()
            }

            async fn identities_of(&self, user_id: i64) -> StoreResult<Vec<Identity>> {
                let rows = sqlx::query(
                    "SELECT provider, subject, user_id, login, created_at FROM identities WHERE user_id = $1 ORDER BY provider",
                )
                .bind(user_id)
                .fetch_all(&self.pool)
                .await?;
                rows.iter().map(Self::identity_row).collect()
            }

            async fn totp(&self, user_id: i64) -> StoreResult<(Option<String>, Vec<String>)> {
                let row = sqlx::query("SELECT totp_secret, recovery_codes FROM users WHERE user_id = $1")
                    .bind(user_id)
                    .fetch_optional(&self.pool)
                    .await?;
                match row {
                    Some(r) => Ok((r.try_get(0)?, words(&r.try_get::<String, _>(1)?))),
                    None => Ok((None, vec![])),
                }
            }

            async fn set_totp(&self, user_id: i64, secret: Option<&str>, recovery: &[String]) -> StoreResult<()> {
                sqlx::query("UPDATE users SET totp_secret = $2, recovery_codes = $3 WHERE user_id = $1")
                    .bind(user_id)
                    .bind(secret)
                    .bind(recovery.join(" "))
                    .execute(&self.pool)
                    .await?;
                Ok(())
            }

            // ───── forks and repository lifecycle ─────

            async fn set_fork_of(&self, repo: RepoId, parent: RepoId) -> StoreResult<()> {
                sqlx::query("UPDATE repos SET fork_of = $2 WHERE repo_id = $1").bind(repo.0).bind(parent.0).execute(&self.pool).await?;
                Ok(())
            }

            async fn fork_of(&self, repo: RepoId) -> StoreResult<Option<RepoId>> {
                let v: Option<Option<i64>> =
                    sqlx::query_scalar("SELECT fork_of FROM repos WHERE repo_id = $1").bind(repo.0).fetch_optional(&self.pool).await?;
                Ok(v.flatten().map(RepoId))
            }

            async fn forks(&self, repo: RepoId) -> StoreResult<Vec<RepoId>> {
                let v: Vec<i64> = sqlx::query_scalar("SELECT repo_id FROM repos WHERE fork_of = $1 ORDER BY repo_id")
                    .bind(repo.0)
                    .fetch_all(&self.pool)
                    .await?;
                Ok(v.into_iter().map(RepoId).collect())
            }

            async fn delete_repo(&self, repo: RepoId) -> StoreResult<()> {
                let _w = self.write_lock().await;
                let mut tx = self.pool.begin_with(Self::BEGIN_WRITE).await?;
                for t in [
                    "refs",
                    "ref_log",
                    "changesets",
                    "changeset_parents",
                    "derived",
                    "git_map",
                    "git_commits",
                    "git_packs",
                    "reviews",
                    "review_events",
                    "repo_grants",
                    "issues",
                    "issue_events",
                    "labels",
                    "item_labels",
                    "item_people",
                    "milestones",
                    "statuses",
                    "notifications",
                    "subscriptions",
                    "lfs_objects",
                    "ci_runs",
                    "ci_jobs",
                ] {
                    sqlx::query(&format!("DELETE FROM {t} WHERE repo_id = $1")).bind(repo.0).execute(&mut *tx).await?;
                }
                sqlx::query("DELETE FROM release_assets WHERE release_id IN (SELECT release_id FROM releases WHERE repo_id = $1)")
                    .bind(repo.0)
                    .execute(&mut *tx)
                    .await?;
                sqlx::query("DELETE FROM releases WHERE repo_id = $1").bind(repo.0).execute(&mut *tx).await?;
                sqlx::query(
                    "DELETE FROM webhook_deliveries WHERE hook_id IN (SELECT hook_id FROM webhooks WHERE scope_kind = 'repo' AND scope_id = $1)",
                )
                .bind(repo.0)
                .execute(&mut *tx)
                .await?;
                sqlx::query("DELETE FROM webhooks WHERE scope_kind = 'repo' AND scope_id = $1").bind(repo.0).execute(&mut *tx).await?;
                sqlx::query("DELETE FROM secrets WHERE scope_kind = 'repo' AND scope_id = $1").bind(repo.0).execute(&mut *tx).await?;
                sqlx::query("UPDATE repos SET fork_of = NULL WHERE fork_of = $1").bind(repo.0).execute(&mut *tx).await?;
                sqlx::query("DELETE FROM repos WHERE repo_id = $1").bind(repo.0).execute(&mut *tx).await?;
                tx.commit().await?;
                Ok(())
            }

            async fn rename_repo(&self, repo: RepoId, name: &str) -> StoreResult<()> {
                let r = sqlx::query("UPDATE repos SET name = $2 WHERE repo_id = $1").bind(repo.0).bind(name).execute(&self.pool).await;
                match r {
                    Ok(_) => Ok(()),
                    Err(sqlx::Error::Database(e)) if e.is_unique_violation() => Err(StoreError::RepoExists(name.into())),
                    Err(e) => Err(e.into()),
                }
            }

            // ───── LFS ─────

            async fn lfs_get(&self, repo: RepoId, oids: &[String]) -> StoreResult<Vec<Option<(ContentId, i64)>>> {
                let mut found = std::collections::HashMap::new();
                for chunk in oids.chunks(IN_CHUNK) {
                    let sql = format!(
                        "SELECT oid, content_id, size FROM lfs_objects WHERE repo_id = $1 AND oid IN ({})",
                        placeholders(2, chunk.len())
                    );
                    let mut q = sqlx::query(&sql).bind(repo.0);
                    for o in chunk {
                        q = q.bind(o.clone());
                    }
                    for r in q.fetch_all(&self.pool).await? {
                        let oid: String = r.try_get(0)?;
                        found.insert(oid, (content(&r.try_get::<Vec<u8>, _>(1)?)?, r.try_get::<i64, _>(2)?));
                    }
                }
                Ok(oids.iter().map(|o| found.get(o).cloned()).collect())
            }

            async fn lfs_put(&self, repo: RepoId, oid: &str, size: i64, c: &ContentId) -> StoreResult<()> {
                sqlx::query(
                    "INSERT INTO lfs_objects (repo_id, oid, size, content_id, created_at) VALUES ($1, $2, $3, $4, $5) \
                     ON CONFLICT (repo_id, oid) DO NOTHING",
                )
                .bind(repo.0)
                .bind(oid)
                .bind(size)
                .bind(c.as_bytes().as_slice())
                .bind(now_ms())
                .execute(&self.pool)
                .await?;
                Ok(())
            }

            // ───── CI ─────

            async fn create_ci_run(&self, run: &CiRun, jobs: &[NewCiJob]) -> StoreResult<(CiRun, Vec<CiJob>)> {
                let _w = self.write_lock().await;
                let mut tx = self.pool.begin_with(Self::BEGIN_WRITE).await?;
                let now = now_ms();
                let row = sqlx::query(
                    "INSERT INTO ci_runs (repo_id, workflow, event, ref_name, cs_id, review, state, actor, created_at, updated_at) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $9) RETURNING run_id",
                )
                .bind(run.repo_id.0)
                .bind(&run.workflow)
                .bind(&run.event)
                .bind(&run.ref_name)
                .bind(run.cs.as_bytes().as_slice())
                .bind(run.review)
                .bind(&run.state)
                .bind(&run.actor)
                .bind(now)
                .fetch_one(&mut *tx)
                .await?;
                let run_id: i64 = row.try_get(0)?;
                let mut out = Vec::new();
                for j in jobs {
                    let state = if j.needs.is_empty() { "queued" } else { "blocked" };
                    let row = sqlx::query(
                        "INSERT INTO ci_jobs (run_id, repo_id, name, needs, labels, spec, state) VALUES ($1, $2, $3, $4, $5, $6, $7) \
                         RETURNING job_id",
                    )
                    .bind(run_id)
                    .bind(run.repo_id.0)
                    .bind(&j.name)
                    .bind(j.needs.join(" "))
                    .bind(j.labels.join(" "))
                    .bind(&j.spec)
                    .bind(state)
                    .fetch_one(&mut *tx)
                    .await?;
                    out.push(CiJob {
                        id: row.try_get(0)?,
                        run_id,
                        repo_id: run.repo_id,
                        name: j.name.clone(),
                        needs: j.needs.clone(),
                        labels: j.labels.clone(),
                        spec: j.spec.clone(),
                        state: state.into(),
                        runner: String::new(),
                        lease_until: 0,
                        started_ms: None,
                        finished_ms: None,
                        exit_code: None,
                    });
                }
                tx.commit().await?;
                Ok((CiRun { id: run_id, created_ms: now, updated_ms: now, ..run.clone() }, out))
            }

            async fn get_ci_run(&self, id: i64) -> StoreResult<Option<CiRun>> {
                let row = sqlx::query(
                    "SELECT run_id, repo_id, workflow, event, ref_name, cs_id, review, state, actor, created_at, updated_at \
                     FROM ci_runs WHERE run_id = $1",
                )
                .bind(id)
                .fetch_optional(&self.pool)
                .await?;
                row.map(|r| Self::ci_run(&r)).transpose()
            }

            async fn list_ci_runs(&self, repo: RepoId, cs: Option<&ChangesetId>, limit: u32) -> StoreResult<Vec<CiRun>> {
                let rows = match cs {
                    Some(c) => {
                        sqlx::query(
                            "SELECT run_id, repo_id, workflow, event, ref_name, cs_id, review, state, actor, created_at, updated_at \
                             FROM ci_runs WHERE repo_id = $1 AND cs_id = $2 ORDER BY run_id DESC LIMIT $3",
                        )
                        .bind(repo.0)
                        .bind(c.as_bytes().as_slice())
                        .bind(limit as i64)
                        .fetch_all(&self.pool)
                        .await?
                    }
                    None => {
                        sqlx::query(
                            "SELECT run_id, repo_id, workflow, event, ref_name, cs_id, review, state, actor, created_at, updated_at \
                             FROM ci_runs WHERE repo_id = $1 ORDER BY run_id DESC LIMIT $2",
                        )
                        .bind(repo.0)
                        .bind(limit as i64)
                        .fetch_all(&self.pool)
                        .await?
                    }
                };
                rows.iter().map(Self::ci_run).collect()
            }

            async fn set_ci_run_state(&self, id: i64, state: &str) -> StoreResult<()> {
                sqlx::query("UPDATE ci_runs SET state = $2, updated_at = $3 WHERE run_id = $1")
                    .bind(id)
                    .bind(state)
                    .bind(now_ms())
                    .execute(&self.pool)
                    .await?;
                Ok(())
            }

            async fn ci_jobs(&self, run_id: i64) -> StoreResult<Vec<CiJob>> {
                let rows = sqlx::query(
                    "SELECT job_id, run_id, repo_id, name, needs, labels, spec, state, runner, lease_until, started_at, finished_at, \
                     exit_code FROM ci_jobs WHERE run_id = $1 ORDER BY job_id",
                )
                .bind(run_id)
                .fetch_all(&self.pool)
                .await?;
                rows.iter().map(Self::ci_job).collect()
            }

            async fn get_ci_job(&self, id: i64) -> StoreResult<Option<CiJob>> {
                let row = sqlx::query(
                    "SELECT job_id, run_id, repo_id, name, needs, labels, spec, state, runner, lease_until, started_at, finished_at, \
                     exit_code FROM ci_jobs WHERE job_id = $1",
                )
                .bind(id)
                .fetch_optional(&self.pool)
                .await?;
                row.map(|r| Self::ci_job(&r)).transpose()
            }

            async fn claim_ci_job(&self, runner: &str, labels: &[String], now: i64, lease_until: i64) -> StoreResult<Option<CiJob>> {
                let rows = sqlx::query(
                    "SELECT job_id, run_id, repo_id, name, needs, labels, spec, state, runner, lease_until, started_at, finished_at, \
                     exit_code FROM ci_jobs WHERE state = 'queued' OR (state = 'running' AND lease_until < $1) ORDER BY job_id LIMIT 100",
                )
                .bind(now)
                .fetch_all(&self.pool)
                .await?;
                for r in &rows {
                    let j = Self::ci_job(r)?;
                    let fits = labels.iter().any(|l| l == "*") || j.labels.iter().all(|l| labels.contains(l));
                    if !fits {
                        continue;
                    }
                    let n = sqlx::query(
                        "UPDATE ci_jobs SET state = 'running', runner = $4, lease_until = $5, started_at = COALESCE(started_at, $6) \
                         WHERE job_id = $1 AND state = $2 AND lease_until = $3",
                    )
                    .bind(j.id)
                    .bind(&j.state)
                    .bind(j.lease_until)
                    .bind(runner)
                    .bind(lease_until)
                    .bind(now)
                    .execute(&self.pool)
                    .await?
                    .rows_affected();
                    if n == 1 {
                        return Ok(Some(CiJob {
                            state: "running".into(),
                            runner: runner.into(),
                            lease_until,
                            started_ms: j.started_ms.or(Some(now)),
                            ..j
                        }));
                    }
                }
                Ok(None)
            }

            async fn set_ci_job(
                &self,
                id: i64,
                runner: &str,
                state: &str,
                exit_code: Option<i64>,
                lease_until: i64,
                now: i64,
            ) -> StoreResult<bool> {
                let finished = matches!(state, "success" | "failure" | "cancelled" | "skipped");
                let n = sqlx::query(
                    "UPDATE ci_jobs SET state = $3, exit_code = COALESCE($4, exit_code), lease_until = $5, \
                     finished_at = COALESCE($6, finished_at) WHERE job_id = $1 AND ($2 = '' OR runner = $2)",
                )
                .bind(id)
                .bind(runner)
                .bind(state)
                .bind(exit_code)
                .bind(lease_until)
                .bind(if finished { Some(now) } else { None })
                .execute(&self.pool)
                .await?
                .rows_affected();
                Ok(n > 0)
            }

            async fn append_ci_log(&self, job_id: i64, seq: i64, data: &[u8]) -> StoreResult<()> {
                sqlx::query("INSERT INTO ci_logs (job_id, seq, data) VALUES ($1, $2, $3) ON CONFLICT (job_id, seq) DO NOTHING")
                    .bind(job_id)
                    .bind(seq)
                    .bind(data)
                    .execute(&self.pool)
                    .await?;
                Ok(())
            }

            async fn ci_log(&self, job_id: i64, after: i64) -> StoreResult<Vec<(i64, Vec<u8>)>> {
                let rows = sqlx::query("SELECT seq, data FROM ci_logs WHERE job_id = $1 AND seq > $2 ORDER BY seq")
                    .bind(job_id)
                    .bind(after)
                    .fetch_all(&self.pool)
                    .await?;
                rows.iter().map(|r| Ok((r.try_get(0)?, r.try_get(1)?))).collect()
            }

            async fn create_ci_runner(&self, name: &str, token_hash: &[u8; 32], labels: &[String]) -> StoreResult<CiRunner> {
                let now = now_ms();
                let row = sqlx::query(
                    "INSERT INTO ci_runners (name, token_hash, labels, created_at) VALUES ($1, $2, $3, $4) RETURNING runner_id",
                )
                .bind(name)
                .bind(token_hash.as_slice())
                .bind(labels.join(" "))
                .bind(now)
                .fetch_one(&self.pool)
                .await?;
                Ok(CiRunner { id: row.try_get(0)?, name: name.into(), labels: labels.to_vec(), created_ms: now, last_seen: None })
            }

            async fn ci_runner_by_token(&self, token_hash: &[u8; 32], now: i64) -> StoreResult<Option<CiRunner>> {
                let row = sqlx::query("SELECT runner_id, name, labels, created_at, last_seen FROM ci_runners WHERE token_hash = $1")
                    .bind(token_hash.as_slice())
                    .fetch_optional(&self.pool)
                    .await?;
                let Some(row) = row else { return Ok(None) };
                let r = Self::ci_runner(&row)?;
                if r.last_seen.is_none_or(|t| now - t > 30_000) {
                    sqlx::query("UPDATE ci_runners SET last_seen = $2 WHERE runner_id = $1").bind(r.id).bind(now).execute(&self.pool).await?;
                }
                Ok(Some(r))
            }

            async fn list_ci_runners(&self) -> StoreResult<Vec<CiRunner>> {
                let rows = sqlx::query("SELECT runner_id, name, labels, created_at, last_seen FROM ci_runners ORDER BY runner_id")
                    .fetch_all(&self.pool)
                    .await?;
                rows.iter().map(Self::ci_runner).collect()
            }

            async fn delete_ci_runner(&self, id: i64) -> StoreResult<bool> {
                Ok(sqlx::query("DELETE FROM ci_runners WHERE runner_id = $1").bind(id).execute(&self.pool).await?.rows_affected() > 0)
            }

            // ───── secrets ─────

            async fn put_secret(&self, scope_kind: &str, scope_id: i64, name: &str, ciphertext: &[u8]) -> StoreResult<()> {
                sqlx::query(
                    "INSERT INTO secrets (scope_kind, scope_id, name, ciphertext, updated_at) VALUES ($1, $2, $3, $4, $5) \
                     ON CONFLICT (scope_kind, scope_id, name) DO UPDATE SET ciphertext = excluded.ciphertext, updated_at = excluded.updated_at",
                )
                .bind(scope_kind)
                .bind(scope_id)
                .bind(name)
                .bind(ciphertext)
                .bind(now_ms())
                .execute(&self.pool)
                .await?;
                Ok(())
            }

            async fn delete_secret(&self, scope_kind: &str, scope_id: i64, name: &str) -> StoreResult<bool> {
                Ok(sqlx::query("DELETE FROM secrets WHERE scope_kind = $1 AND scope_id = $2 AND name = $3")
                    .bind(scope_kind)
                    .bind(scope_id)
                    .bind(name)
                    .execute(&self.pool)
                    .await?
                    .rows_affected()
                    > 0)
            }

            async fn secrets(&self, scope_kind: &str, scope_id: i64) -> StoreResult<Vec<(String, Vec<u8>, i64)>> {
                let rows = sqlx::query(
                    "SELECT name, ciphertext, updated_at FROM secrets WHERE scope_kind = $1 AND scope_id = $2 ORDER BY name",
                )
                .bind(scope_kind)
                .bind(scope_id)
                .fetch_all(&self.pool)
                .await?;
                rows.iter().map(|r| Ok((r.try_get(0)?, r.try_get(1)?, r.try_get(2)?))).collect()
            }
        }
    };
}

collab_store!(SqliteStore, sqlx::Sqlite);
collab_store!(PostgresStore, sqlx::Postgres);
