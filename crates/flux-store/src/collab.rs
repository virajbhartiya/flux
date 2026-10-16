//! The collaboration store contract: issues, labels, milestones, releases,
//! commit statuses, webhooks, SSH keys, notifications, external identities,
//! two-factor auth, LFS objects, CI and secrets.
//!
//! Issues and reviews share one number space per repository, so imported
//! GitHub numbers (issues and pull requests) keep their meaning.

use async_trait::async_trait;
use flux_core::{ChangesetId, ContentId};

use crate::meta::{RepoId, Review, StoreResult};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    pub number: i64,
    pub title: String,
    pub body: String,
    pub author: String,
    /// `open` or `closed`.
    pub state: String,
    /// `completed`, `not_planned`, `duplicate` or empty.
    pub state_reason: String,
    pub milestone: Option<i64>,
    pub locked: bool,
    pub comments: i64,
    pub created_ms: i64,
    pub updated_ms: i64,
    pub closed_ms: Option<i64>,
}

/// A new issue. `number` and the timestamps are set by importers only.
#[derive(Debug, Clone, Default)]
pub struct NewIssue {
    pub number: Option<i64>,
    pub title: String,
    pub body: String,
    pub author: String,
    pub state: String,
    pub state_reason: String,
    pub milestone: Option<i64>,
    pub created_ms: Option<i64>,
    pub updated_ms: Option<i64>,
    pub closed_ms: Option<i64>,
}

#[derive(Debug, Clone, Default)]
pub struct IssueUpdate {
    pub title: Option<String>,
    pub body: Option<String>,
    pub state: Option<String>,
    pub state_reason: Option<String>,
    pub milestone: Option<Option<i64>>,
    pub locked: Option<bool>,
}

/// Filters for listing issues. Empty fields match everything.
#[derive(Debug, Clone, Default)]
pub struct IssueQuery {
    pub state: Option<String>,
    pub label: Option<String>,
    pub assignee: Option<String>,
    pub author: Option<String>,
    pub milestone: Option<i64>,
    /// Case-insensitive substring of the title or body.
    pub text: Option<String>,
    /// `created`, `updated` (default) or `comments`.
    pub sort: Option<String>,
    pub limit: u32,
    pub offset: u32,
}

/// A comment or timeline event on an issue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueEvent {
    pub seq: i64,
    pub number: i64,
    /// `comment`, `closed`, `reopened`, `labeled`, `unlabeled`, `assigned`,
    /// `unassigned`, `renamed`, `milestoned`, `locked`, `unlocked`, ...
    pub kind: String,
    pub author: String,
    pub body: String,
    pub created_ms: i64,
    pub updated_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Label {
    pub name: String,
    /// Six hex digits, no `#`.
    pub color: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Milestone {
    pub number: i64,
    pub title: String,
    pub description: String,
    pub state: String,
    pub due_ms: Option<i64>,
    pub created_ms: i64,
    pub closed_ms: Option<i64>,
    pub open_items: i64,
    pub closed_items: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub id: i64,
    pub tag: String,
    pub name: String,
    pub body: String,
    pub draft: bool,
    pub prerelease: bool,
    pub author: String,
    pub target: Option<ChangesetId>,
    pub created_ms: i64,
    pub published_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseAsset {
    pub id: i64,
    pub release_id: i64,
    pub name: String,
    pub content: ContentId,
    pub size: i64,
    pub content_type: String,
    pub downloads: i64,
    pub created_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitStatus {
    pub context: String,
    /// `pending`, `success`, `failure` or `error`.
    pub state: String,
    pub description: String,
    pub target_url: String,
    pub creator: String,
    pub created_ms: i64,
    pub updated_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Webhook {
    pub id: i64,
    /// `repo` or `org`.
    pub scope_kind: String,
    pub scope_id: i64,
    pub url: String,
    pub secret: String,
    /// Event names, or `*` for all.
    pub events: Vec<String>,
    pub active: bool,
    pub created_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delivery {
    pub id: i64,
    pub hook_id: i64,
    pub event: String,
    pub guid: String,
    pub payload: String,
    /// `pending`, `ok` or `failed`.
    pub state: String,
    pub attempts: i64,
    pub next_ms: i64,
    pub status_code: i64,
    pub response: String,
    pub duration_ms: i64,
    pub created_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshKey {
    pub id: i64,
    pub user_id: i64,
    pub title: String,
    /// `SHA256:<base64>` as printed by `ssh-keygen -l`.
    pub fingerprint: String,
    pub public_key: String,
    pub created_ms: i64,
    pub last_used: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    pub id: i64,
    pub repo_id: RepoId,
    pub number: i64,
    pub kind: String,
    pub reason: String,
    pub title: String,
    pub unread: bool,
    pub updated_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub provider: String,
    pub subject: String,
    pub user_id: i64,
    pub login: String,
    pub created_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CiRun {
    pub id: i64,
    pub repo_id: RepoId,
    pub workflow: String,
    pub event: String,
    pub ref_name: String,
    pub cs: ChangesetId,
    pub review: Option<i64>,
    pub state: String,
    pub actor: String,
    pub created_ms: i64,
    pub updated_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CiJob {
    pub id: i64,
    pub run_id: i64,
    pub repo_id: RepoId,
    pub name: String,
    /// Names of jobs this one waits for.
    pub needs: Vec<String>,
    /// Runner labels the job needs (`runs-on`).
    pub labels: Vec<String>,
    /// The job definition (JSON).
    pub spec: String,
    pub state: String,
    pub runner: String,
    pub lease_until: i64,
    pub started_ms: Option<i64>,
    pub finished_ms: Option<i64>,
    pub exit_code: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct NewCiJob {
    pub name: String,
    pub needs: Vec<String>,
    pub labels: Vec<String>,
    pub spec: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CiRunner {
    pub id: i64,
    pub name: String,
    pub labels: Vec<String>,
    pub created_ms: i64,
    pub last_seen: Option<i64>,
}

/// A review imported with its original number and timestamps.
#[derive(Debug, Clone)]
pub struct ImportedReview {
    pub number: i64,
    pub title: String,
    pub body: String,
    pub author: String,
    pub head: ChangesetId,
    pub target: Vec<u8>,
    pub state: String,
    pub landed: Option<ChangesetId>,
    pub draft: bool,
    pub created_ms: i64,
    pub updated_ms: i64,
}

#[async_trait]
pub trait CollabStore: Send + Sync + 'static {
    // ───── issues ─────
    /// Creates an issue with the next number (shared with reviews) unless
    /// `number` is given (imports; replaces nothing: an existing number is
    /// a conflict).
    async fn create_issue(&self, repo: RepoId, issue: &NewIssue) -> StoreResult<Issue>;
    async fn get_issue(&self, repo: RepoId, number: i64) -> StoreResult<Option<Issue>>;
    async fn list_issues(&self, repo: RepoId, q: &IssueQuery) -> StoreResult<(Vec<Issue>, i64)>;
    async fn update_issue(&self, repo: RepoId, number: i64, u: &IssueUpdate) -> StoreResult<Option<Issue>>;
    /// Issue counts by state: (open, closed).
    async fn issue_counts(&self, repo: RepoId) -> StoreResult<(i64, i64)>;
    /// Append a timeline entry. `external_id` makes imports idempotent:
    /// a second insert with the same id returns the existing entry.
    async fn add_issue_event(
        &self,
        repo: RepoId,
        number: i64,
        kind: &str,
        author: &str,
        body: &str,
        created_ms: Option<i64>,
        external_id: Option<&str>,
    ) -> StoreResult<IssueEvent>;
    async fn issue_events(&self, repo: RepoId, number: i64) -> StoreResult<Vec<IssueEvent>>;
    async fn get_issue_event(&self, repo: RepoId, seq: i64) -> StoreResult<Option<IssueEvent>>;
    async fn update_issue_event(&self, repo: RepoId, seq: i64, body: &str) -> StoreResult<bool>;
    async fn delete_issue_event(&self, repo: RepoId, seq: i64) -> StoreResult<bool>;
    /// The next free number in the shared issue/review space.
    async fn next_item_number(&self, repo: RepoId) -> StoreResult<i64>;
    /// Is `number` an issue, a review, or free?
    async fn item_kind(&self, repo: RepoId, number: i64) -> StoreResult<Option<&'static str>>;

    // ───── reviews (extensions) ─────
    async fn import_review(&self, repo: RepoId, r: &ImportedReview) -> StoreResult<Review>;
    /// Append an imported review timeline entry with its original time.
    /// Returns false when `external_id` was imported before.
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
    ) -> StoreResult<bool>;
    async fn set_review_draft(&self, repo: RepoId, number: i64, draft: bool) -> StoreResult<()>;
    async fn review_flags(&self, repo: RepoId, number: i64) -> StoreResult<(bool, i64)>;
    async fn set_review_source(&self, repo: RepoId, number: i64, source: RepoId) -> StoreResult<()>;
    async fn review_count(&self, repo: RepoId, state: &str) -> StoreResult<i64>;

    // ───── labels, people, milestones ─────
    async fn put_label(&self, repo: RepoId, label: &Label) -> StoreResult<()>;
    async fn rename_label(&self, repo: RepoId, old: &str, label: &Label) -> StoreResult<bool>;
    async fn delete_label(&self, repo: RepoId, name: &str) -> StoreResult<bool>;
    async fn list_labels(&self, repo: RepoId) -> StoreResult<Vec<Label>>;
    async fn set_item_labels(&self, repo: RepoId, number: i64, labels: &[String]) -> StoreResult<()>;
    /// Labels of several items: (number, label) pairs.
    async fn item_labels(&self, repo: RepoId, numbers: &[i64]) -> StoreResult<Vec<(i64, String)>>;
    /// `kind`: `assignee` or `reviewer`.
    async fn set_item_people(&self, repo: RepoId, number: i64, kind: &str, names: &[String]) -> StoreResult<()>;
    async fn item_people(&self, repo: RepoId, numbers: &[i64], kind: &str) -> StoreResult<Vec<(i64, String)>>;
    async fn create_milestone(
        &self,
        repo: RepoId,
        number: Option<i64>,
        title: &str,
        description: &str,
        due_ms: Option<i64>,
    ) -> StoreResult<Milestone>;
    async fn update_milestone(
        &self,
        repo: RepoId,
        number: i64,
        title: &str,
        description: &str,
        state: &str,
        due_ms: Option<i64>,
    ) -> StoreResult<bool>;
    async fn delete_milestone(&self, repo: RepoId, number: i64) -> StoreResult<bool>;
    async fn list_milestones(&self, repo: RepoId) -> StoreResult<Vec<Milestone>>;

    // ───── releases ─────
    async fn create_release(&self, repo: RepoId, r: &Release) -> StoreResult<Release>;
    async fn update_release(&self, repo: RepoId, r: &Release) -> StoreResult<bool>;
    async fn delete_release(&self, repo: RepoId, id: i64) -> StoreResult<bool>;
    async fn get_release(&self, repo: RepoId, id: i64) -> StoreResult<Option<Release>>;
    async fn release_by_tag(&self, repo: RepoId, tag: &str) -> StoreResult<Option<Release>>;
    async fn list_releases(&self, repo: RepoId, limit: u32) -> StoreResult<Vec<Release>>;
    async fn add_release_asset(&self, a: &ReleaseAsset) -> StoreResult<ReleaseAsset>;
    async fn release_assets(&self, release_id: i64) -> StoreResult<Vec<ReleaseAsset>>;
    async fn delete_release_asset(&self, release_id: i64, id: i64) -> StoreResult<bool>;
    async fn count_asset_download(&self, id: i64) -> StoreResult<()>;

    // ───── commit statuses ─────
    async fn put_status(&self, repo: RepoId, cs: &ChangesetId, s: &CommitStatus) -> StoreResult<()>;
    async fn statuses(&self, repo: RepoId, cs: &ChangesetId) -> StoreResult<Vec<CommitStatus>>;
    /// Statuses for several changesets at once (log pages, review lists).
    async fn statuses_many(&self, repo: RepoId, ids: &[ChangesetId]) -> StoreResult<Vec<(ChangesetId, CommitStatus)>>;

    // ───── webhooks ─────
    async fn create_webhook(&self, h: &Webhook) -> StoreResult<Webhook>;
    async fn update_webhook(&self, h: &Webhook) -> StoreResult<bool>;
    async fn delete_webhook(&self, id: i64) -> StoreResult<bool>;
    async fn get_webhook(&self, id: i64) -> StoreResult<Option<Webhook>>;
    async fn list_webhooks(&self, scope_kind: &str, scope_id: i64) -> StoreResult<Vec<Webhook>>;
    async fn enqueue_delivery(&self, hook_id: i64, event: &str, guid: &str, payload: &str) -> StoreResult<i64>;
    /// Claim up to `limit` due deliveries: their `next_at` moves to
    /// `lease_until` so concurrent workers (or instances) skip them.
    async fn claim_deliveries(&self, now_ms: i64, lease_until: i64, limit: u32) -> StoreResult<Vec<Delivery>>;
    async fn finish_delivery(
        &self,
        id: i64,
        state: &str,
        next_ms: i64,
        status_code: i64,
        response: &str,
        duration_ms: i64,
    ) -> StoreResult<()>;
    async fn list_deliveries(&self, hook_id: i64, limit: u32) -> StoreResult<Vec<Delivery>>;
    async fn get_delivery(&self, id: i64) -> StoreResult<Option<Delivery>>;
    /// Drop finished deliveries older than `before_ms`.
    async fn prune_deliveries(&self, before_ms: i64) -> StoreResult<u64>;

    // ───── SSH keys ─────
    async fn add_ssh_key(&self, user_id: i64, title: &str, fingerprint: &str, public_key: &str) -> StoreResult<SshKey>;
    async fn list_ssh_keys(&self, user_id: i64) -> StoreResult<Vec<SshKey>>;
    async fn delete_ssh_key(&self, user_id: i64, id: i64) -> StoreResult<bool>;
    async fn ssh_key_by_fingerprint(&self, fingerprint: &str) -> StoreResult<Option<SshKey>>;
    async fn touch_ssh_key(&self, id: i64, now_ms: i64) -> StoreResult<()>;

    // ───── notifications and subscriptions ─────
    /// Insert or refresh (mark unread, bump time) a notification.
    async fn notify(&self, user_id: i64, repo: RepoId, number: i64, kind: &str, reason: &str, title: &str) -> StoreResult<()>;
    async fn notifications(&self, user_id: i64, unread_only: bool, limit: u32) -> StoreResult<Vec<Notification>>;
    async fn unread_count(&self, user_id: i64) -> StoreResult<i64>;
    /// `id = None` marks everything read.
    async fn mark_read(&self, user_id: i64, id: Option<i64>) -> StoreResult<u64>;
    /// `number = 0` is the whole repository. `state = None` clears.
    async fn set_subscription(&self, user_id: i64, repo: RepoId, number: i64, state: Option<&str>) -> StoreResult<()>;
    async fn subscription(&self, user_id: i64, repo: RepoId, number: i64) -> StoreResult<Option<String>>;
    /// (user id, state) for subscribers of an item and of its repository.
    async fn subscribers(&self, repo: RepoId, number: i64) -> StoreResult<Vec<(i64, i64, String)>>;

    // ───── accounts (extensions) ─────
    async fn user_kind(&self, user_id: i64) -> StoreResult<String>;
    async fn set_user_kind(&self, user_id: i64, kind: &str) -> StoreResult<()>;
    async fn link_identity(&self, provider: &str, subject: &str, user_id: i64, login: &str) -> StoreResult<()>;
    async fn unlink_identity(&self, user_id: i64, provider: &str) -> StoreResult<bool>;
    async fn identity(&self, provider: &str, subject: &str) -> StoreResult<Option<Identity>>;
    async fn identities_of(&self, user_id: i64) -> StoreResult<Vec<Identity>>;
    /// (base32 secret, recovery code hashes). The secret is `None` when
    /// two-factor auth is off.
    async fn totp(&self, user_id: i64) -> StoreResult<(Option<String>, Vec<String>)>;
    async fn set_totp(&self, user_id: i64, secret: Option<&str>, recovery: &[String]) -> StoreResult<()>;

    // ───── forks ─────
    async fn set_fork_of(&self, repo: RepoId, parent: RepoId) -> StoreResult<()>;
    async fn fork_of(&self, repo: RepoId) -> StoreResult<Option<RepoId>>;
    async fn forks(&self, repo: RepoId) -> StoreResult<Vec<RepoId>>;
    async fn delete_repo(&self, repo: RepoId) -> StoreResult<()>;
    async fn rename_repo(&self, repo: RepoId, name: &str) -> StoreResult<()>;

    // ───── LFS ─────
    async fn lfs_get(&self, repo: RepoId, oids: &[String]) -> StoreResult<Vec<Option<(ContentId, i64)>>>;
    async fn lfs_put(&self, repo: RepoId, oid: &str, size: i64, content: &ContentId) -> StoreResult<()>;

    // ───── CI ─────
    /// Creates a run with its jobs. Jobs without `needs` start `queued`,
    /// the rest `blocked`.
    async fn create_ci_run(&self, run: &CiRun, jobs: &[NewCiJob]) -> StoreResult<(CiRun, Vec<CiJob>)>;
    async fn get_ci_run(&self, id: i64) -> StoreResult<Option<CiRun>>;
    async fn list_ci_runs(&self, repo: RepoId, cs: Option<&ChangesetId>, limit: u32) -> StoreResult<Vec<CiRun>>;
    async fn set_ci_run_state(&self, id: i64, state: &str) -> StoreResult<()>;
    async fn ci_jobs(&self, run_id: i64) -> StoreResult<Vec<CiJob>>;
    async fn get_ci_job(&self, id: i64) -> StoreResult<Option<CiJob>>;
    /// Atomically claim the oldest queued job whose labels the runner has
    /// (or whose lease expired while running).
    async fn claim_ci_job(&self, runner: &str, labels: &[String], now_ms: i64, lease_until: i64) -> StoreResult<Option<CiJob>>;
    /// Update a job's state; `runner` must match unless empty.
    async fn set_ci_job(
        &self,
        id: i64,
        runner: &str,
        state: &str,
        exit_code: Option<i64>,
        lease_until: i64,
        now_ms: i64,
    ) -> StoreResult<bool>;
    async fn append_ci_log(&self, job_id: i64, seq: i64, data: &[u8]) -> StoreResult<()>;
    /// Log chunks with `seq > after`, in order.
    async fn ci_log(&self, job_id: i64, after: i64) -> StoreResult<Vec<(i64, Vec<u8>)>>;
    async fn create_ci_runner(&self, name: &str, token_hash: &[u8; 32], labels: &[String]) -> StoreResult<CiRunner>;
    async fn ci_runner_by_token(&self, token_hash: &[u8; 32], now_ms: i64) -> StoreResult<Option<CiRunner>>;
    async fn list_ci_runners(&self) -> StoreResult<Vec<CiRunner>>;
    async fn delete_ci_runner(&self, id: i64) -> StoreResult<bool>;

    // ───── secrets ─────
    async fn put_secret(&self, scope_kind: &str, scope_id: i64, name: &str, ciphertext: &[u8]) -> StoreResult<()>;
    async fn delete_secret(&self, scope_kind: &str, scope_id: i64, name: &str) -> StoreResult<bool>;
    /// (name, ciphertext, updated)
    async fn secrets(&self, scope_kind: &str, scope_id: i64) -> StoreResult<Vec<(String, Vec<u8>, i64)>>;
}
