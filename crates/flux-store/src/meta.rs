//! The metadata store contract (SPEC §5.1).

use async_trait::async_trait;
use flux_core::{ChangesetId, ContentId};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RepoId(pub i64);

#[derive(Debug, Clone)]
pub struct RepoInfo {
    pub id: RepoId,
    pub name: String,
    pub config: String,
}

/// Monotonic, gap-free per-repo log position.
pub type LogSeq = i64;

/// A changeset ready to be persisted. `gen` is computed by the caller from
/// the parents' generations; `body` is the canonical encoding.
#[derive(Debug, Clone)]
pub struct NewChangeset {
    pub id: ChangesetId,
    pub parents: Vec<ChangesetId>,
    pub gen_num: u64,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct StoredChangeset {
    pub id: ChangesetId,
    pub gen_num: u64,
    pub log_seq: LogSeq,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefValue {
    pub target: ChangesetId,
    pub version: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefExpect {
    /// Unconditional (admin only).
    Any,
    /// The ref must not exist.
    Absent,
    Version(u64),
    Target(ChangesetId),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CasOutcome {
    Updated { version: u64 },
    Deleted,
    Mismatch { current: Option<RefValue> },
}

#[derive(Debug, Clone)]
pub struct RefLogEntry {
    pub actor: String,
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContentMeta {
    pub size: u64,
    /// Git blob hash (`sha1("blob <len>\0" + data)`).
    pub git_sha1: [u8; 20],
    pub sha256: [u8; 32],
    pub is_binary: bool,
}

/// A cached git pack for a repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitPack {
    /// The pack bytes, stored as content.
    pub content: ContentId,
    pub objects: u32,
    /// Ref heads the pack covers (everything reachable from them).
    pub tips: Vec<ChangesetId>,
    pub created_ms: i64,
}

/// One node of the commit graph, in log order (parents always precede
/// children).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphRow {
    pub seq: LogSeq,
    pub id: ChangesetId,
    pub gen_num: u64,
    pub parents: Vec<ChangesetId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefLogRow {
    pub seq: i64,
    pub name: Vec<u8>,
    pub old: Option<ChangesetId>,
    pub new: Option<ChangesetId>,
    pub actor: String,
    pub reason: String,
    pub ts_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    pub id: i64,
    pub name: String,
    pub is_admin: bool,
}

/// A user account with profile fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub user: User,
    pub email: Option<String>,
    pub display_name: String,
    pub disabled: bool,
    pub created_at: i64,
}

#[derive(Debug, Clone, Default)]
pub struct NewAccount {
    pub name: String,
    pub email: Option<String>,
    pub display_name: String,
    /// PHC string (argon2id); `None` for token-only accounts.
    pub password_hash: Option<String>,
    pub is_admin: bool,
}

/// Fields to change on an account; `None` leaves a field as is.
#[derive(Debug, Clone, Default)]
pub struct AccountUpdate {
    pub display_name: Option<String>,
    pub email: Option<Option<String>>,
    pub password_hash: Option<Option<String>>,
    pub is_admin: Option<bool>,
    pub disabled: Option<bool>,
}

/// An API token or web session, as listed to its owner (never the secret).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenInfo {
    /// First 8 bytes of the token hash, hex: a stable public handle.
    pub id: String,
    pub label: String,
    /// `api` or `session`.
    pub kind: String,
    /// `all`, or a space-separated list such as `read write`.
    pub scopes: String,
    pub created_at: i64,
    pub expires_at: Option<i64>,
    pub last_used: Option<i64>,
}

/// The user behind a valid token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenAuth {
    pub user: User,
    pub kind: String,
    pub scopes: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Org {
    pub id: i64,
    pub name: String,
    pub display_name: String,
    pub description: String,
    /// Role every member gets on the org's repositories: `none`, `read` or `write`.
    pub base_role: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Team {
    pub id: i64,
    pub org_id: i64,
    pub name: String,
    pub description: String,
}

/// Ownership and visibility of a repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoAccess {
    /// `user`, `org`, or empty for repositories created before ownership.
    pub owner_kind: String,
    pub owner_id: i64,
    /// `public`, `internal` (any signed-in user may read) or `private`.
    pub visibility: String,
    /// Role every signed-in user gets (`write` for pre-ownership repositories).
    pub signed_in_role: String,
}

/// A role on a repository granted to a user or a team.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub kind: String,
    pub subject_id: i64,
    pub subject_name: String,
    pub role: String,
}

/// Everything needed to compute a user's role on a repository.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuthzFacts {
    /// The user's role in the owning organization.
    pub org_role: Option<String>,
    pub org_base_role: Option<String>,
    /// Direct and team grants.
    pub grants: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditEntry {
    pub seq: i64,
    pub ts: i64,
    pub actor: String,
    pub action: String,
    pub target: String,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Review {
    pub number: i64,
    pub title: String,
    pub body: String,
    pub author: String,
    /// Tip of the proposed stack.
    pub head: ChangesetId,
    /// Ref the stack lands on.
    pub target: Vec<u8>,
    /// `open`, `landed` or `closed`.
    pub state: String,
    pub landed: Option<ChangesetId>,
    pub created_ms: i64,
    pub updated_ms: i64,
}

#[derive(Debug, Clone, Default)]
pub struct ReviewUpdate {
    pub title: Option<String>,
    pub body: Option<String>,
    pub head: Option<ChangesetId>,
    pub state: Option<String>,
    pub landed: Option<ChangesetId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewEvent {
    pub seq: i64,
    /// `comment`, `updated`, `landed`, `closed`, `reopened`, `approved`.
    pub kind: String,
    pub author: String,
    pub body: String,
    pub cs: Option<ChangesetId>,
    pub path: Option<Vec<u8>>,
    pub line: Option<i64>,
    pub ts_ms: i64,
}

/// Small stable integer identifying a derived-data type (SPEC §6).
pub type DerivedTypeId = i32;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("repository {0:?} not found")]
    RepoNotFound(String),
    #[error("repository {0:?} already exists")]
    RepoExists(String),
    #[error("corrupt data: {0}")]
    Corrupt(String),
    /// A unique name or email is already taken.
    #[error("{0}")]
    Conflict(String),
    #[error(transparent)]
    Backend(#[from] anyhow::Error),
}

impl From<sqlx::Error> for StoreError {
    fn from(e: sqlx::Error) -> Self {
        StoreError::Backend(e.into())
    }
}

impl From<object_store::Error> for StoreError {
    fn from(e: object_store::Error) -> Self {
        StoreError::Backend(e.into())
    }
}

pub type StoreResult<T> = Result<T, StoreError>;

#[async_trait]
pub trait MetaStore: Send + Sync + 'static {
    // repos
    async fn create_repo(&self, name: &str) -> StoreResult<RepoId>;
    async fn get_repo(&self, name: &str) -> StoreResult<Option<RepoInfo>>;
    async fn list_repos(&self) -> StoreResult<Vec<RepoInfo>>;
    /// Replace a repository's JSON config (description, default branch,
    /// path ACL rules, ...).
    async fn set_repo_config(&self, repo: RepoId, config: &str) -> StoreResult<()>;

    // changesets
    /// Insert changesets in order. Idempotent: existing ids keep their
    /// original log position. Returns the log position of each input.
    async fn put_changesets(&self, repo: RepoId, batch: &[NewChangeset]) -> StoreResult<Vec<LogSeq>>;
    async fn get_changesets(&self, repo: RepoId, ids: &[ChangesetId]) -> StoreResult<Vec<Option<StoredChangeset>>>;
    /// Generation numbers only (cheaper than full bodies).
    async fn get_gens(&self, repo: RepoId, ids: &[ChangesetId]) -> StoreResult<Vec<Option<u64>>>;
    async fn log_tail(&self, repo: RepoId, after: LogSeq, limit: u32) -> StoreResult<Vec<(LogSeq, ChangesetId)>>;
    async fn children(&self, repo: RepoId, id: &ChangesetId) -> StoreResult<Vec<ChangesetId>>;
    /// Up to `limit` changeset ids whose bytes start with the given prefix
    /// (`nibbles` hex digits; odd counts allowed).
    async fn resolve_prefix(&self, repo: RepoId, hex_prefix: &str, limit: u32) -> StoreResult<Vec<ChangesetId>>;

    // refs
    async fn get_ref(&self, repo: RepoId, name: &[u8]) -> StoreResult<Option<RefValue>>;
    /// Compare-and-swap. `new = None` deletes the ref.
    async fn cas_ref(
        &self,
        repo: RepoId,
        name: &[u8],
        expect: RefExpect,
        new: Option<ChangesetId>,
        log: RefLogEntry,
    ) -> StoreResult<CasOutcome>;
    async fn list_refs(&self, repo: RepoId, prefix: &[u8], limit: u32) -> StoreResult<Vec<(Vec<u8>, RefValue)>>;

    // derived data
    async fn get_derived(&self, repo: RepoId, ty: DerivedTypeId, ids: &[ChangesetId]) -> StoreResult<Vec<Option<Vec<u8>>>>;
    async fn put_derived(&self, repo: RepoId, ty: DerivedTypeId, entries: &[(ChangesetId, Vec<u8>)]) -> StoreResult<()>;
    /// Every changeset that has a value of this derived type.
    async fn derived_ids(&self, repo: RepoId, ty: DerivedTypeId) -> StoreResult<Vec<ChangesetId>>;

    // content metadata (global across repos: content is content-addressed)
    async fn get_content_meta(&self, ids: &[ContentId]) -> StoreResult<Vec<Option<ContentMeta>>>;
    async fn put_content_meta(&self, metas: &[(ContentId, ContentMeta)]) -> StoreResult<()>;

    // git mapping
    async fn git_map_put(&self, repo: RepoId, entries: &[(ChangesetId, [u8; 20])]) -> StoreResult<()>;
    async fn git_to_cs(&self, repo: RepoId, shas: &[[u8; 20]]) -> StoreResult<Vec<Option<ChangesetId>>>;
    async fn cs_to_git(&self, repo: RepoId, ids: &[ChangesetId]) -> StoreResult<Vec<Option<[u8; 20]>>>;

    // git frontend mappings
    async fn git_trees_get(&self, ids: &[flux_core::TreeId]) -> StoreResult<Vec<Option<[u8; 20]>>>;
    async fn git_trees_put(&self, pairs: &[(flux_core::TreeId, [u8; 20])]) -> StoreResult<()>;
    async fn git_tree_by_sha(&self, sha: &[u8; 20]) -> StoreResult<Option<flux_core::TreeId>>;
    async fn git_commits_put(&self, repo: RepoId, pairs: &[(ChangesetId, [u8; 20])]) -> StoreResult<()>;
    async fn git_commits_lookup(&self, repo: RepoId, shas: &[[u8; 20]]) -> StoreResult<Vec<Option<ChangesetId>>>;
    async fn git_commits_of(&self, repo: RepoId, ids: &[ChangesetId]) -> StoreResult<Vec<Option<[u8; 20]>>>;
    async fn content_by_sha1(&self, shas: &[[u8; 20]]) -> StoreResult<Vec<Option<ContentId>>>;
    async fn git_pack_get(&self, repo: RepoId) -> StoreResult<Option<GitPack>>;
    /// Replace the repository's cached pack.
    async fn git_pack_put(&self, repo: RepoId, pack: &GitPack) -> StoreResult<()>;

    // commit graph
    /// Changesets with `log_seq > after`, in log order, with their parents.
    async fn graph_tail(&self, repo: RepoId, after: LogSeq, limit: u32) -> StoreResult<Vec<GraphRow>>;

    // ref log
    /// Newest first; `before` is an exclusive upper bound on `seq`.
    async fn ref_log(&self, repo: RepoId, before: Option<i64>, limit: u32) -> StoreResult<Vec<RefLogRow>>;

    // users and tokens
    async fn create_user(&self, name: &str, is_admin: bool) -> StoreResult<User>;
    async fn get_user(&self, name: &str) -> StoreResult<Option<User>>;
    async fn list_users(&self) -> StoreResult<Vec<User>>;
    async fn add_token(&self, user_id: i64, token_hash: &[u8; 32], label: &str) -> StoreResult<()>;
    async fn user_for_token(&self, token_hash: &[u8; 32]) -> StoreResult<Option<User>>;
    async fn revoke_tokens(&self, user_id: i64) -> StoreResult<u64>;

    // accounts (user and organization names share one namespace)
    async fn create_account(&self, a: &NewAccount) -> StoreResult<User>;
    /// By name or email; also returns the password hash.
    async fn get_account(&self, name_or_email: &str) -> StoreResult<Option<(Account, Option<String>)>>;
    async fn get_account_by_id(&self, user_id: i64) -> StoreResult<Option<Account>>;
    async fn update_account(&self, user_id: i64, u: &AccountUpdate) -> StoreResult<()>;
    async fn list_accounts(&self) -> StoreResult<Vec<Account>>;
    async fn user_count(&self) -> StoreResult<i64>;

    // tokens and sessions
    async fn add_token_ext(
        &self,
        user_id: i64,
        token_hash: &[u8; 32],
        label: &str,
        kind: &str,
        scopes: &str,
        expires_at: Option<i64>,
    ) -> StoreResult<()>;
    /// The user for a token that exists, has not expired and belongs to an
    /// enabled account.
    async fn token_auth(&self, token_hash: &[u8; 32], now_ms: i64) -> StoreResult<Option<TokenAuth>>;
    async fn touch_token(&self, token_hash: &[u8; 32], now_ms: i64) -> StoreResult<()>;
    async fn list_tokens(&self, user_id: i64) -> StoreResult<Vec<TokenInfo>>;
    /// Revoke one token by its public id. Returns whether it existed.
    async fn revoke_token(&self, user_id: i64, id: &str) -> StoreResult<bool>;
    async fn revoke_token_hash(&self, token_hash: &[u8; 32]) -> StoreResult<()>;

    // organizations and teams
    async fn create_org(&self, name: &str, display_name: &str, owner_user_id: i64) -> StoreResult<Org>;
    async fn get_org(&self, name: &str) -> StoreResult<Option<Org>>;
    async fn get_org_by_id(&self, org_id: i64) -> StoreResult<Option<Org>>;
    async fn list_orgs(&self) -> StoreResult<Vec<Org>>;
    async fn orgs_of_user(&self, user_id: i64) -> StoreResult<Vec<(Org, String)>>;
    async fn update_org(&self, org_id: i64, display_name: &str, description: &str, base_role: &str) -> StoreResult<()>;
    async fn org_members(&self, org_id: i64) -> StoreResult<Vec<(User, String)>>;
    /// Set a member's role (`owner` or `member`), or remove them (`None`,
    /// which also removes them from the org's teams).
    async fn set_org_member(&self, org_id: i64, user_id: i64, role: Option<&str>) -> StoreResult<()>;
    async fn create_team(&self, org_id: i64, name: &str, description: &str) -> StoreResult<Team>;
    async fn delete_team(&self, team_id: i64) -> StoreResult<()>;
    async fn list_teams(&self, org_id: i64) -> StoreResult<Vec<Team>>;
    async fn team_members(&self, team_id: i64) -> StoreResult<Vec<User>>;
    async fn set_team_member(&self, team_id: i64, user_id: i64, member: bool) -> StoreResult<()>;

    // repository ownership, visibility and grants
    async fn create_repo_owned(
        &self,
        name: &str,
        owner_kind: &str,
        owner_id: i64,
        visibility: &str,
        signed_in_role: &str,
    ) -> StoreResult<RepoId>;
    async fn repo_access(&self, repo: RepoId) -> StoreResult<Option<RepoAccess>>;
    async fn set_repo_visibility(&self, repo: RepoId, visibility: &str, signed_in_role: &str) -> StoreResult<()>;
    async fn set_repo_grant(&self, repo: RepoId, kind: &str, subject_id: i64, role: Option<&str>) -> StoreResult<()>;
    async fn repo_grants(&self, repo: RepoId) -> StoreResult<Vec<Grant>>;
    /// Repositories with a grant to a team.
    async fn team_repos(&self, team_id: i64) -> StoreResult<Vec<(RepoId, String)>>;
    async fn authz_facts(&self, repo: RepoId, user_id: i64) -> StoreResult<AuthzFacts>;

    // audit log
    async fn audit_append(&self, actor: &str, action: &str, target: &str, detail: &str) -> StoreResult<()>;
    /// Newest first; `before` is an exclusive upper bound on `seq`.
    async fn audit_list(&self, before: Option<i64>, limit: u32) -> StoreResult<Vec<AuditEntry>>;

    // reviews
    /// Creates a review with the next number in the repo.
    async fn create_review(
        &self,
        repo: RepoId,
        title: &str,
        body: &str,
        author: &str,
        head: ChangesetId,
        target: &[u8],
    ) -> StoreResult<Review>;
    async fn get_review(&self, repo: RepoId, number: i64) -> StoreResult<Option<Review>>;
    /// Newest first, optionally filtered by state.
    async fn list_reviews(&self, repo: RepoId, state: Option<&str>, limit: u32) -> StoreResult<Vec<Review>>;
    async fn update_review(&self, repo: RepoId, number: i64, update: ReviewUpdate) -> StoreResult<Option<Review>>;
    async fn add_review_event(
        &self,
        repo: RepoId,
        number: i64,
        kind: &str,
        author: &str,
        body: &str,
        cs: Option<ChangesetId>,
        path: Option<&[u8]>,
        line: Option<i64>,
    ) -> StoreResult<i64>;
    async fn review_events(&self, repo: RepoId, number: i64) -> StoreResult<Vec<ReviewEvent>>;

    // operations
    /// Write a consistent snapshot of the metadata to `path` (SQLite: an
    /// online `VACUUM INTO`). Postgres deployments use pg_dump/PITR instead.
    async fn backup_to(&self, path: &std::path::Path) -> StoreResult<()>;
    /// Number of changesets in a repository.
    async fn changeset_count(&self, repo: RepoId) -> StoreResult<u64>;
}

/// Turn a hex prefix into an inclusive-exclusive byte range `[lo, hi)`.
/// `hi` is `None` when the range is unbounded above.
pub(crate) fn prefix_range(hex_prefix: &str) -> Option<(Vec<u8>, Option<Vec<u8>>)> {
    if hex_prefix.is_empty() || hex_prefix.len() > 64 || !hex_prefix.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut padded_lo = hex_prefix.to_ascii_lowercase();
    let odd = padded_lo.len() % 2 == 1;
    if odd {
        padded_lo.push('0');
    }
    let lo = hex::decode(&padded_lo).ok()?;
    // Increment the prefix at nibble granularity.
    let mut hi = lo.clone();
    let step: u16 = if odd { 0x10 } else { 0x01 };
    let mut carry = step;
    for b in hi.iter_mut().rev() {
        let v = *b as u16 + carry;
        *b = (v & 0xff) as u8;
        carry = v >> 8;
        if carry == 0 {
            break;
        }
    }
    let hi = if carry != 0 { None } else { Some(hi) };
    Some((lo, hi))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_ranges() {
        assert_eq!(prefix_range("ab"), Some((vec![0xab], Some(vec![0xac]))));
        assert_eq!(prefix_range("abc"), Some((vec![0xab, 0xc0], Some(vec![0xab, 0xd0]))));
        assert_eq!(prefix_range("ff"), Some((vec![0xff], None)));
        assert_eq!(prefix_range("f"), Some((vec![0xf0], None)));
        assert_eq!(prefix_range("0f"), Some((vec![0x0f], Some(vec![0x10]))));
        assert_eq!(prefix_range("zz"), None);
    }
}
