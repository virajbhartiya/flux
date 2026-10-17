//! Server-side operations, independent of the RPC layer (SPEC §7).

use std::collections::HashSet;
use std::sync::Arc;

use flux_core::{Changeset, ChangesetId, ContentId, FileChange, FileKind, RepoPath, TreeEntry, TreeId};
use flux_derive::{
    DeriveError, TREE_V1, TreeSource, TreeWriter, derive_tree, derive_tree_buffered, diff_trees_in, lookup_path, lookup_path_in,
};
use flux_proto::v1;
use flux_store::{
    BlobStore, CasOutcome, ContentMeta, MetaStore, NewChangeset, RefExpect, RefLogEntry, RefValue, RepoId, StoreError,
};
use thiserror::Error;
use tonic::Status;

/// Changesets prepared earlier in a create stream, possibly not committed
/// yet: id → (generation, root tree).
pub type InFlight = std::collections::HashMap<ChangesetId, (u64, TreeId)>;

/// A validated batch whose trees are written; see [`Flux::prepare_changesets`].
pub struct PreparedBatch {
    batch: Vec<Changeset>,
    rows: Vec<NewChangeset>,
    trees: Vec<TreeId>,
    t0: std::time::Instant,
    phases: Vec<(&'static str, u128)>,
}

impl PreparedBatch {
    /// (id, generation, root tree) of each changeset, in order.
    pub fn prepared(&self) -> impl Iterator<Item = (ChangesetId, u64, TreeId)> + '_ {
        self.rows.iter().zip(&self.trees).map(|(r, t)| (r.id, r.gen_num, *t))
    }
}

#[derive(Debug, Error)]
pub enum FluxError {
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    InvalidArgument(String),
    #[error("{0}")]
    FailedPrecondition(String),
    #[error("{0}")]
    AlreadyExists(String),
    #[error("{0}")]
    Unauthenticated(String),
    #[error("{0}")]
    PermissionDenied(String),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl From<StoreError> for FluxError {
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::RepoNotFound(r) => FluxError::NotFound(format!("repository {r:?} not found")),
            StoreError::RepoExists(r) => FluxError::AlreadyExists(format!("repository {r:?} already exists")),
            StoreError::Conflict(m) => FluxError::AlreadyExists(m),
            other => FluxError::Internal(other.into()),
        }
    }
}

impl From<DeriveError> for FluxError {
    fn from(e: DeriveError) -> Self {
        match e {
            DeriveError::Core(c) => FluxError::InvalidArgument(c.to_string()),
            other => FluxError::Internal(other.into()),
        }
    }
}

impl From<flux_core::Error> for FluxError {
    fn from(e: flux_core::Error) -> Self {
        FluxError::InvalidArgument(e.to_string())
    }
}

impl From<FluxError> for Status {
    fn from(e: FluxError) -> Self {
        match e {
            FluxError::NotFound(m) => Status::not_found(m),
            FluxError::InvalidArgument(m) => Status::invalid_argument(m),
            FluxError::FailedPrecondition(m) => Status::failed_precondition(m),
            FluxError::AlreadyExists(m) => Status::already_exists(m),
            FluxError::Unauthenticated(m) => Status::unauthenticated(m),
            FluxError::PermissionDenied(m) => Status::permission_denied(m),
            FluxError::Internal(e) => {
                tracing::error!(error = ?e, "internal error");
                Status::internal(e.to_string())
            }
        }
    }
}

pub type FluxResult<T> = Result<T, FluxError>;

/// A loaded changeset with its stored metadata.
#[derive(Debug, Clone)]
pub struct Loaded {
    pub id: ChangesetId,
    pub gen_num: u64,
    pub log_seq: i64,
    pub canonical: Vec<u8>,
    pub changeset: Changeset,
}

/// Something that happened in a repository (streamed to watchers).
#[derive(Debug, Clone)]
pub enum Event {
    RefMoved {
        repo: String,
        name: Vec<u8>,
        old: Option<ChangesetId>,
        new: Option<ChangesetId>,
        actor: String,
    },
    Changesets {
        repo: String,
        count: usize,
        last: ChangesetId,
    },
    Review {
        repo: String,
        number: i64,
        kind: String,
        actor: String,
    },
    /// An issue changed: `opened`, `closed`, `reopened`, `edited`,
    /// `labeled`, `assigned`, ...
    Issue {
        repo: String,
        number: i64,
        action: String,
        actor: String,
    },
    /// A comment on an issue or review (`on`: `issue` | `review`).
    Comment {
        repo: String,
        number: i64,
        on: &'static str,
        seq: i64,
        actor: String,
        body: String,
    },
    Status {
        repo: String,
        cs: ChangesetId,
        context: String,
        state: String,
        actor: String,
    },
    Release {
        repo: String,
        id: i64,
        action: String,
        actor: String,
    },
    /// A CI run changed state.
    Ci {
        repo: String,
        run: i64,
        state: String,
    },
}

impl Event {
    pub fn repo(&self) -> &str {
        match self {
            Event::RefMoved { repo, .. }
            | Event::Changesets { repo, .. }
            | Event::Review { repo, .. }
            | Event::Issue { repo, .. }
            | Event::Comment { repo, .. }
            | Event::Status { repo, .. }
            | Event::Release { repo, .. }
            | Event::Ci { repo, .. } => repo,
        }
    }
}

pub struct Flux {
    pub meta: Arc<dyn MetaStore>,
    pub blobs: BlobStore,
    repos: moka::future::Cache<String, RepoId>,
    repo_names: moka::sync::Cache<RepoId, String>,
    /// Immutable changesets, by (repo, id).
    changesets: moka::sync::Cache<(RepoId, ChangesetId), Arc<Loaded>>,
    pub(crate) graphs: std::sync::Mutex<std::collections::HashMap<RepoId, Arc<crate::history::RepoGraph>>>,
    pub(crate) lastmod_locks: std::sync::Mutex<std::collections::HashMap<RepoId, Arc<tokio::sync::Mutex<()>>>>,
    pub(crate) lastmod_done: std::sync::Mutex<std::collections::HashMap<RepoId, Arc<std::sync::Mutex<HashSet<ChangesetId>>>>>,
    pub(crate) blame_cache: moka::sync::Cache<(RepoId, ChangesetId, Vec<u8>), Arc<crate::history::Blame>>,
    events: tokio::sync::broadcast::Sender<Event>,
    derived_cache: moka::sync::Cache<(RepoId, i32, ChangesetId), [u8; 32]>,
    pub(crate) repo_configs: moka::sync::Cache<RepoId, Arc<crate::repo_config::RepoConfig>>,
    pub(crate) git: Arc<crate::git::objects::GitCaches>,
    pub auth_mode: crate::auth::AuthMode,
    /// Who may create accounts (the first account is always allowed).
    pub signup: crate::identity::SignupPolicy,
    pub(crate) token_cache: moka::sync::Cache<[u8; 32], flux_store::TokenAuth>,
    /// Effective roles by (user id or 0, repository); see `rbac`.
    pub(crate) role_cache: moka::sync::Cache<(i64, RepoId), crate::rbac::Role>,
    pub(crate) access_cache: moka::sync::Cache<RepoId, Arc<flux_store::RepoAccess>>,
    /// Recent failed sign-ins by account name and by client, for throttling.
    pub(crate) login_failures: moka::sync::Cache<String, u32>,
    /// Repositories with lastmod derivation pending: (tip, first write, last write).
    pub(crate) lastmod_pending:
        std::sync::Mutex<std::collections::HashMap<RepoId, (ChangesetId, std::time::Instant, std::time::Instant)>>,
    pub(crate) ahead_behind_cache: moka::sync::Cache<(RepoId, ChangesetId, ChangesetId), (usize, usize)>,
    pub(crate) file_lists: moka::sync::Cache<TreeId, crate::search::FileList>,
    /// Search corpora by root tree (see `search`).
    pub(crate) search_corpora: moka::sync::Cache<TreeId, crate::search::Corpus>,
    /// Server-wide settings (public URL, webhooks, secrets key, sign-in providers).
    pub settings: crate::settings::Settings,
    /// Events for the background reactor (webhooks, notifications, CI).
    /// Unbounded, so nothing is dropped when a burst outpaces it.
    pub(crate) reactor: std::sync::OnceLock<tokio::sync::mpsc::UnboundedSender<Event>>,
    /// Wakes the webhook delivery worker.
    pub(crate) hook_wake: Arc<tokio::sync::Notify>,
    /// Wakes CI runners waiting for jobs.
    pub(crate) ci_wake: Arc<tokio::sync::Notify>,
    /// Tags created by a push of many tags at once, which (as on GitHub)
    /// trigger no CI runs: (repository, flux ref name) → target.
    pub(crate) ci_quiet_tags: moka::sync::Cache<(i64, Vec<u8>), ChangesetId>,
    /// Two-factor enrollments awaiting confirmation, by user id.
    pub(crate) pending_totp: moka::sync::Cache<i64, crate::twofactor::Enrollment>,
    /// Last TOTP step accepted per user (codes work once).
    pub(crate) totp_used: moka::sync::Cache<i64, i64>,
    /// Sign-ins waiting for a second factor, and OAuth flows in progress.
    pub tickets: moka::sync::Cache<String, crate::sso::Ticket>,
    /// Recovery codes waiting for their one-time display.
    pub recovery_display: moka::sync::Cache<String, Vec<String>>,
    /// API requests per caller in the current minute: (window, count).
    pub(crate) rate: moka::sync::Cache<String, (i64, u32)>,
    /// Email queue (set when the reactor starts; `None` without SMTP).
    pub(crate) mailer: std::sync::OnceLock<Option<tokio::sync::mpsc::Sender<crate::mail::Mail>>>,
}

impl Flux {
    pub fn new(meta: Arc<dyn MetaStore>, blobs: BlobStore) -> Self {
        Self {
            meta,
            blobs,
            repos: moka::future::Cache::new(10_000),
            repo_names: moka::sync::Cache::new(10_000),
            changesets: moka::sync::Cache::builder()
                .weigher(|_, l: &Arc<Loaded>| (l.canonical.len() * 2 + 256) as u32)
                .max_capacity(256 << 20)
                .build(),
            graphs: Default::default(),
            lastmod_locks: Default::default(),
            lastmod_done: Default::default(),
            blame_cache: moka::sync::Cache::builder()
                .weigher(|_, b: &Arc<crate::history::Blame>| (b.ranges.len() * 48 + 64) as u32)
                .max_capacity(64 << 20)
                .build(),
            events: tokio::sync::broadcast::channel(4096).0,
            derived_cache: moka::sync::Cache::new(2_000_000),
            git: Arc::new(crate::git::objects::GitCaches::new()),
            repo_configs: moka::sync::Cache::builder()
                .time_to_live(std::time::Duration::from_secs(5))
                .max_capacity(10_000)
                .build(),
            auth_mode: crate::auth::AuthMode::Open,
            signup: Default::default(),
            token_cache: moka::sync::Cache::builder()
                .max_capacity(100_000)
                .time_to_live(std::time::Duration::from_secs(60))
                .build(),
            role_cache: moka::sync::Cache::builder()
                .max_capacity(1_000_000)
                .time_to_live(std::time::Duration::from_secs(300))
                .build(),
            access_cache: moka::sync::Cache::builder()
                .max_capacity(100_000)
                .time_to_live(std::time::Duration::from_secs(300))
                .build(),
            ahead_behind_cache: moka::sync::Cache::new(100_000),
            lastmod_pending: Default::default(),
            login_failures: moka::sync::Cache::builder()
                .max_capacity(100_000)
                .time_to_live(std::time::Duration::from_secs(900))
                .build(),
            file_lists: moka::sync::Cache::builder()
                .weigher(|_, l: &crate::search::FileList| (l.len() * 80 + 64) as u32)
                .max_capacity(256 << 20)
                .build(),
            search_corpora: moka::sync::Cache::builder()
                .weigher(|_, c: &crate::search::Corpus| {
                    c.iter().map(|(p, d)| d.len() + p.as_bytes().len() + 48).sum::<usize>().min(u32::MAX as usize) as u32
                })
                .max_capacity(1 << 30)
                .time_to_idle(std::time::Duration::from_secs(3600))
                .build(),
            settings: Default::default(),
            reactor: Default::default(),
            hook_wake: Default::default(),
            ci_wake: Default::default(),
            ci_quiet_tags: moka::sync::Cache::builder().time_to_live(std::time::Duration::from_secs(600)).build(),
            pending_totp: moka::sync::Cache::builder().time_to_live(std::time::Duration::from_secs(900)).build(),
            totp_used: moka::sync::Cache::builder().time_to_live(std::time::Duration::from_secs(300)).build(),
            tickets: moka::sync::Cache::builder().time_to_live(std::time::Duration::from_secs(600)).max_capacity(100_000).build(),
            recovery_display: moka::sync::Cache::builder().time_to_live(std::time::Duration::from_secs(300)).build(),
            rate: moka::sync::Cache::builder().time_to_idle(std::time::Duration::from_secs(120)).max_capacity(1_000_000).build(),
            mailer: Default::default(),
        }
    }

    pub fn with_settings(mut self, settings: crate::settings::Settings) -> Self {
        self.settings = settings;
        self
    }

    pub fn with_signup(mut self, signup: crate::identity::SignupPolicy) -> Self {
        self.signup = signup;
        self
    }

    pub fn with_auth_mode(mut self, mode: crate::auth::AuthMode) -> Self {
        self.auth_mode = mode;
        self
    }

    /// A single 32-byte derived value, cached in memory (derived data is
    /// immutable once written).
    pub async fn derived_get(&self, repo: RepoId, ty: i32, id: &ChangesetId) -> FluxResult<Option<[u8; 32]>> {
        let key = (repo, ty, *id);
        if let Some(v) = self.derived_cache.get(&key) {
            return Ok(Some(v));
        }
        let v = self.meta.get_derived(repo, ty, std::slice::from_ref(id)).await?.pop().flatten();
        match v.and_then(|v| <[u8; 32]>::try_from(v.as_slice()).ok()) {
            Some(v) => {
                self.derived_cache.insert(key, v);
                Ok(Some(v))
            }
            None => Ok(None),
        }
    }

    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    pub fn publish(&self, e: Event) {
        if let Some(tx) = self.reactor.get() {
            let _ = tx.send(e.clone());
        }
        let _ = self.events.send(e);
    }

    /// Name of a repository id (for events).
    pub async fn repo_name(&self, repo: RepoId) -> String {
        if let Some(n) = self.repo_names.get(&repo) {
            return n;
        }
        if let Ok(repos) = self.meta.list_repos().await {
            for r in repos {
                self.repo_names.insert(r.id, r.name);
            }
        }
        self.repo_names.get(&repo).unwrap_or_default()
    }

    /// Drop cached state for a deleted or renamed repository.
    pub async fn forget_repo(&self, name: &str, id: RepoId) {
        self.repos.invalidate(name).await;
        self.repo_names.invalidate(&id);
        self.repo_configs.invalidate(&id);
        self.access_cache.invalidate(&id);
        self.role_cache.invalidate_all();
        self.graphs.lock().unwrap().remove(&id);
    }

    pub async fn repo(&self, name: &str) -> FluxResult<RepoId> {
        if let Some(id) = self.repos.get(name).await {
            return Ok(id);
        }
        let info =
            self.meta.get_repo(name).await?.ok_or_else(|| FluxError::NotFound(format!("repository {name:?} not found")))?;
        self.repos.insert(name.to_string(), info.id).await;
        self.repo_names.insert(info.id, name.to_string());
        Ok(info.id)
    }

    pub async fn create_repo(&self, name: &str) -> FluxResult<RepoId> {
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || "-_./".contains(c)) {
            return Err(FluxError::InvalidArgument(format!("invalid repository name {name:?}")));
        }
        Ok(self.meta.create_repo(name).await?)
    }

    // ───────────── content ─────────────

    /// Store content. The blob is durable before its metadata row exists, so
    /// a metadata row implies the blob is readable (SPEC §5.2).
    pub async fn put_content(&self, data: &[u8]) -> FluxResult<(ContentId, ContentMeta)> {
        Ok(self.put_contents(vec![(bytes::Bytes::copy_from_slice(data), None)]).await?.pop().unwrap())
    }

    /// Batch form with optional delta hints (previous version of each path).
    pub async fn put_contents(&self, items: Vec<(bytes::Bytes, Option<ContentId>)>) -> FluxResult<Vec<(ContentId, ContentMeta)>> {
        let (t, n) = (std::time::Instant::now(), items.len());
        let out = self.blobs.put_contents(items).await?;
        let stored = t.elapsed().as_micros();
        self.blobs.sync().await?;
        let synced = t.elapsed().as_micros();
        self.meta.put_content_meta(&out).await?;
        tracing::debug!(n, stored, synced, total = t.elapsed().as_micros() as u64, "put_contents");
        Ok(out)
    }

    // ───────────── changesets ─────────────

    /// Load changesets (cached: they are immutable).
    pub async fn load_arc(&self, repo: RepoId, ids: &[ChangesetId]) -> FluxResult<Vec<Option<Arc<Loaded>>>> {
        let mut out: Vec<Option<Arc<Loaded>>> = ids.iter().map(|id| self.changesets.get(&(repo, *id))).collect();
        let missing: Vec<ChangesetId> = ids.iter().zip(&out).filter(|(_, o)| o.is_none()).map(|(i, _)| *i).collect();
        if missing.is_empty() {
            return Ok(out);
        }
        let stored = self.meta.get_changesets(repo, &missing).await?;
        let mut found = std::collections::HashMap::new();
        for s in stored.into_iter().flatten() {
            let changeset = Changeset::decode_verified(&s.body, &s.id)
                .map_err(|e| FluxError::Internal(anyhow::anyhow!("corrupt changeset {}: {e}", s.id)))?;
            let l = Arc::new(Loaded { id: s.id, gen_num: s.gen_num, log_seq: s.log_seq, canonical: s.body, changeset });
            self.changesets.insert((repo, s.id), l.clone());
            found.insert(s.id, l);
        }
        for (slot, id) in out.iter_mut().zip(ids) {
            if slot.is_none() {
                *slot = found.get(id).cloned();
            }
        }
        Ok(out)
    }

    pub async fn load(&self, repo: RepoId, ids: &[ChangesetId]) -> FluxResult<Vec<Option<Loaded>>> {
        Ok(self.load_arc(repo, ids).await?.into_iter().map(|o| o.map(|a| (*a).clone())).collect())
    }

    pub async fn load_one_arc(&self, repo: RepoId, id: &ChangesetId) -> FluxResult<Arc<Loaded>> {
        self.load_arc(repo, std::slice::from_ref(id))
            .await?
            .pop()
            .flatten()
            .ok_or_else(|| FluxError::NotFound(format!("changeset {id} not found")))
    }

    pub async fn load_one(&self, repo: RepoId, id: &ChangesetId) -> FluxResult<Loaded> {
        Ok((*self.load_one_arc(repo, id).await?).clone())
    }

    /// Root tree of a changeset, deriving it (and any underived ancestors) if
    /// the mapping is missing, e.g. after a crash between insert and derive.
    /// Root trees of many changesets: one metadata query for the derived
    /// ones, `root_tree` (which derives) for the rest.
    pub async fn root_trees(&self, repo: RepoId, ids: &[ChangesetId]) -> FluxResult<Vec<TreeId>> {
        let found = self.meta.get_derived(repo, TREE_V1, ids).await?;
        let mut out = Vec::with_capacity(ids.len());
        for (id, v) in ids.iter().zip(found) {
            out.push(match v {
                Some(v) => TreeId::from_slice(&v).map_err(|e| FluxError::Internal(e.into()))?,
                None => self.root_tree(repo, id).await?,
            });
        }
        Ok(out)
    }

    pub async fn root_tree(&self, repo: RepoId, id: &ChangesetId) -> FluxResult<TreeId> {
        if let Some(v) = self.derived_get(repo, TREE_V1, id).await? {
            return Ok(TreeId::from_bytes(v));
        }
        // Walk first parents down to a derived frontier, then derive upwards.
        let mut chain = vec![self.load_one(repo, id).await?];
        let mut base: Option<TreeId> = None;
        while let Some(p) = chain.last().unwrap().changeset.parents.first().copied() {
            if let Some(Some(v)) = self.meta.get_derived(repo, TREE_V1, &[p]).await?.pop() {
                base = Some(TreeId::from_slice(&v).map_err(|e| FluxError::Internal(e.into()))?);
                break;
            }
            chain.push(self.load_one(repo, &p).await?);
        }
        for cs in chain.into_iter().rev() {
            let tree = derive_tree(&self.blobs, base, &cs.changeset.file_changes).await?;
            self.meta.put_derived(repo, TREE_V1, &[(cs.id, tree.as_bytes().to_vec())]).await?;
            base = Some(tree);
        }
        Ok(base.expect("chain is non-empty"))
    }

    /// Validate, persist and derive a new changeset (SPEC §7.1). Idempotent.
    pub async fn create_changeset(self: &Arc<Self>, repo: RepoId, cs: Changeset) -> FluxResult<v1::ChangesetInfo> {
        Ok(self.create_changesets(repo, vec![cs]).await?.pop().unwrap())
    }

    /// Batch form: changesets are applied in order and may use earlier
    /// members of the batch as parents. The whole batch shares one content
    /// check, one concurrent tree flush and one metadata transaction.
    pub async fn create_changesets(self: &Arc<Self>, repo: RepoId, batch: Vec<Changeset>) -> FluxResult<Vec<v1::ChangesetInfo>> {
        self.create_changesets_mapped(repo, batch, &[]).await
    }

    /// [`Flux::create_changesets`] that also records the git commit each
    /// changeset came from (`git_shas` empty, or one per changeset).
    pub async fn create_changesets_mapped(
        self: &Arc<Self>,
        repo: RepoId,
        batch: Vec<Changeset>,
        git_shas: &[[u8; 20]],
    ) -> FluxResult<Vec<v1::ChangesetInfo>> {
        let prepared = self.prepare_changesets(repo, batch, git_shas, &InFlight::new()).await?;
        self.commit_changesets(repo, prepared).await
    }

    /// Validate a batch, derive and write its trees, and run the merge
    /// checks: everything but the metadata commit. `in_flight` holds
    /// changesets prepared by earlier batches of the same stream that may
    /// not be committed yet; they may be used as parents, and
    /// [`Flux::commit_changesets`] must then be called in stream order.
    pub async fn prepare_changesets(
        self: &Arc<Self>,
        repo: RepoId,
        batch: Vec<Changeset>,
        git_shas: &[[u8; 20]],
        in_flight: &InFlight,
    ) -> FluxResult<PreparedBatch> {
        if batch.is_empty() {
            return Ok(PreparedBatch { batch, rows: vec![], trees: vec![], t0: std::time::Instant::now(), phases: vec![] });
        }
        if batch.len() > 10_000 {
            return Err(FluxError::InvalidArgument("at most 10000 changesets per batch".into()));
        }
        if !git_shas.is_empty() && git_shas.len() != batch.len() {
            return Err(FluxError::InvalidArgument("git_sha1s must be empty or match changesets".into()));
        }
        let t0 = std::time::Instant::now();
        let mut phases: Vec<(&'static str, u128)> = Vec::new();
        let mark = |name: &'static str, phases: &mut Vec<(&'static str, u128)>| phases.push((name, t0.elapsed().as_micros()));
        let ids: Vec<ChangesetId> = batch.iter().map(Changeset::id).collect();
        let mut in_batch: std::collections::HashMap<ChangesetId, usize> = std::collections::HashMap::new();
        for (i, (cs, id)) in batch.iter().zip(&ids).enumerate() {
            cs.validate()?;
            for p in &cs.parents {
                if ids[i..].contains(p) {
                    return Err(FluxError::InvalidArgument(format!("parent {p} must precede its child in the batch")));
                }
            }
            in_batch.insert(*id, i);
        }

        // External parents must exist (or be in flight in this stream).
        let mut gens: std::collections::HashMap<ChangesetId, u64> = std::collections::HashMap::new();
        let mut trees: std::collections::HashMap<ChangesetId, TreeId> = std::collections::HashMap::new();
        let mut external: Vec<ChangesetId> = Vec::new();
        for p in batch.iter().flat_map(|c| c.parents.iter()).filter(|p| !in_batch.contains_key(p)) {
            if let Some((g, t)) = in_flight.get(p) {
                gens.insert(*p, *g);
                trees.insert(*p, *t);
            } else if !gens.contains_key(p) {
                gens.insert(*p, 0);
                external.push(*p);
            }
        }
        for (p, g) in external.iter().zip(self.meta.get_gens(repo, &external).await?) {
            match g {
                Some(g) => {
                    gens.insert(*p, g);
                }
                None => return Err(FluxError::FailedPrecondition(format!("CONTENT_MISSING: parent {p} not found"))),
            }
        }

        mark("parents", &mut phases);
        // Referenced contents must exist and sizes must agree.
        let mut wanted: Vec<(&RepoPath, ContentId, u64)> = Vec::new();
        for cs in &batch {
            for (p, c) in &cs.file_changes {
                if let FileChange::Change { content_id, size, kind, .. } = c
                    && !matches!(kind, FileKind::GitLink(_))
                {
                    wanted.push((p, *content_id, *size));
                }
            }
        }
        let unique: Vec<ContentId> = wanted.iter().map(|w| w.1).collect::<HashSet<_>>().into_iter().collect();
        let metas: std::collections::HashMap<ContentId, Option<ContentMeta>> =
            unique.iter().copied().zip(self.meta.get_content_meta(&unique).await?).collect();
        for (path, id, size) in &wanted {
            match metas[id] {
                None => return Err(FluxError::FailedPrecondition(format!("CONTENT_MISSING: {id} (for {path})"))),
                Some(m) if m.size != *size => {
                    return Err(FluxError::InvalidArgument(format!(
                        "size mismatch for {path}: declared {size}, stored {}",
                        m.size
                    )));
                }
                _ => {}
            }
        }

        // Derive trees (buffered), then flush them before any row refers to them.
        mark("contents", &mut phases);
        let writer = TreeWriter::new(&self.blobs);
        let mut rows = Vec::with_capacity(batch.len());
        let mut out_trees = Vec::with_capacity(batch.len());
        for (i, (cs, id)) in batch.iter().zip(&ids).enumerate() {
            let base = match cs.parents.first() {
                None => None,
                Some(p) => Some(match trees.get(p) {
                    Some(t) => *t,
                    None => self.root_tree(repo, p).await?,
                }),
            };
            let tree = derive_tree_buffered(&writer, base, &cs.file_changes).await?;
            trees.insert(*id, tree);
            let gen_num = 1 + cs.parents.iter().map(|p| gens.get(p).copied().unwrap_or(0)).max().unwrap_or(0);
            gens.insert(*id, gen_num);
            rows.push(NewChangeset {
                id: *id,
                parents: cs.parents.clone(),
                gen_num,
                body: cs.encode(),
                derived: vec![(TREE_V1, tree.as_bytes().to_vec())],
                git_sha1: git_shas.get(i).copied(),
            });
            out_trees.push(tree);
        }
        mark("derive", &mut phases);
        // Merge checks are CPU-heavy tree diffs: run them on all cores while
        // the new trees flush, reading the not-yet-written ones from memory.
        // Rejected batches leave only unreferenced (content-addressed) trees.
        let src = TreeSource::with_overlay(&self.blobs, writer.snapshot());
        let trees = Arc::new(trees);
        let mut tasks = tokio::task::JoinSet::new();
        for (cs, id) in batch.iter().zip(&ids) {
            let needs = cs.parents.len() > 1
                || cs.file_changes.values().any(|c| matches!(c, FileChange::Change { copy_from: Some(_), .. }));
            if !needs {
                continue;
            }
            let (this, cs, id, trees, src) = (self.clone(), cs.clone(), *id, trees.clone(), src.clone());
            tasks.spawn(async move { this.check_against_parents(repo, &id, &cs, &trees, &src).await });
        }
        let checks = async {
            while let Some(r) = tasks.join_next().await {
                r.map_err(|e| FluxError::Internal(e.into()))??;
            }
            FluxResult::Ok(())
        };
        let (flushed, checked) = tokio::join!(writer.flush(), checks);
        flushed?;
        checked?;
        mark("flush+check", &mut phases);
        Ok(PreparedBatch { batch, rows, trees: out_trees, t0, phases })
    }

    /// Persist a prepared batch: one metadata transaction for the
    /// changesets, their root trees and git mappings.
    pub async fn commit_changesets(self: &Arc<Self>, repo: RepoId, p: PreparedBatch) -> FluxResult<Vec<v1::ChangesetInfo>> {
        let PreparedBatch { batch, rows, trees: out_trees, t0, mut phases } = p;
        if batch.is_empty() {
            return Ok(vec![]);
        }
        let mark = |name: &'static str, phases: &mut Vec<(&'static str, u128)>| phases.push((name, t0.elapsed().as_micros()));
        mark("queued", &mut phases);
        self.blobs.sync().await?;
        mark("sync", &mut phases);
        let seqs = self.meta.put_changesets(repo, &rows).await?;
        mark("insert", &mut phases);
        tracing::debug!(n = batch.len(), ?phases, "create_changesets");
        for (row, tree) in rows.iter().zip(&out_trees) {
            self.derived_cache.insert((repo, TREE_V1, row.id), *tree.as_bytes());
        }
        self.after_create(repo, &rows, &seqs).await;

        Ok(batch
            .iter()
            .zip(rows)
            .zip(out_trees)
            .zip(seqs)
            .map(|(((cs, row), tree), seq)| v1::ChangesetInfo {
                id: row.id.as_bytes().to_vec(),
                generation: row.gen_num,
                root_tree: tree.as_bytes().to_vec(),
                log_seq: seq,
                canonical: row.body,
                changeset: Some(flux_proto::changeset_to_pb(cs)),
                git_sha1: vec![],
            })
            .collect())
    }

    /// Validation rules that need the parents' trees (SPEC §3.3):
    /// * merge completeness: every path where the parents disagree with
    ///   `parents[0]` must be listed in `file_changes`;
    /// * `copy_from` must name an existing file in the referenced parent.
    async fn check_against_parents(
        &self,
        repo: RepoId,
        id: &ChangesetId,
        cs: &Changeset,
        batch_trees: &std::collections::HashMap<ChangesetId, TreeId>,
        src_trees: &TreeSource,
    ) -> FluxResult<()> {
        let needs_copy_check = cs.file_changes.values().any(|c| matches!(c, FileChange::Change { copy_from: Some(_), .. }));
        if cs.parents.len() < 2 && !needs_copy_check {
            return Ok(());
        }
        let mut parent_trees = Vec::with_capacity(cs.parents.len());
        for p in &cs.parents {
            parent_trees.push(match batch_trees.get(p) {
                Some(t) => *t,
                None => self.root_tree(repo, p).await?,
            });
        }
        for (path, change) in &cs.file_changes {
            if let FileChange::Change { copy_from: Some((from, idx)), .. } = change {
                let found = lookup_path_in(src_trees, parent_trees[*idx as usize], from).await?;
                if found.is_none_or(|e| e.is_dir()) {
                    return Err(FluxError::InvalidArgument(format!("{path}: copy_from {from} does not exist in parent #{idx}")));
                }
            }
        }
        let all = |_: &RepoPath, _: bool| true;
        for pt in &parent_trees[1..] {
            for d in diff_trees_in(src_trees, Some(parent_trees[0]), Some(*pt), &all).await? {
                let covered = cs.file_changes.contains_key(&d.path) || {
                    let mut anc = d.path.parent();
                    let mut hit = false;
                    while let Some(a) = anc {
                        if matches!(cs.file_changes.get(&a), Some(FileChange::Change { .. })) {
                            hit = true;
                            break;
                        }
                        anc = a.parent();
                    }
                    hit
                };
                if !covered {
                    return Err(FluxError::InvalidArgument(format!(
                        "MERGE_INCOMPLETE: merge {id} must list {} (its parents disagree there; SPEC §3.3)",
                        d.path
                    )));
                }
            }
        }
        Ok(())
    }

    /// Batched [`Flux::info`]: one derived-tree and one git-map query.
    pub async fn infos(&self, repo: RepoId, items: &[Arc<Loaded>]) -> FluxResult<Vec<v1::ChangesetInfo>> {
        let ids: Vec<ChangesetId> = items.iter().map(|l| l.id).collect();
        let trees = self.meta.get_derived(repo, TREE_V1, &ids).await?;
        let gits = self.meta.cs_to_git(repo, &ids).await?;
        let mut out = Vec::with_capacity(items.len());
        for ((l, t), g) in items.iter().zip(trees).zip(gits) {
            let tree = match t {
                Some(t) => TreeId::from_slice(&t).map_err(|e| FluxError::Internal(e.into()))?,
                None => self.root_tree(repo, &l.id).await?,
            };
            out.push(v1::ChangesetInfo {
                id: l.id.as_bytes().to_vec(),
                generation: l.gen_num,
                root_tree: tree.as_bytes().to_vec(),
                log_seq: l.log_seq,
                canonical: l.canonical.clone(),
                changeset: Some(flux_proto::changeset_to_pb(&l.changeset)),
                git_sha1: g.map(|g| g.to_vec()).unwrap_or_default(),
            });
        }
        Ok(out)
    }

    pub async fn info(&self, repo: RepoId, loaded: &Loaded) -> FluxResult<v1::ChangesetInfo> {
        let tree = self.root_tree(repo, &loaded.id).await?;
        let git = self.meta.cs_to_git(repo, &[loaded.id]).await?.pop().flatten();
        Ok(v1::ChangesetInfo {
            id: loaded.id.as_bytes().to_vec(),
            generation: loaded.gen_num,
            root_tree: tree.as_bytes().to_vec(),
            log_seq: loaded.log_seq,
            canonical: loaded.canonical.clone(),
            changeset: Some(flux_proto::changeset_to_pb(&loaded.changeset)),
            git_sha1: git.map(|g| g.to_vec()).unwrap_or_default(),
        })
    }

    // ───────────── revisions ─────────────

    /// Resolve `base(~N|^N)*`. Base: `git:<sha>`, a ref name, `cs_<hex>` or a
    /// hex prefix (≥ 6 chars).
    pub async fn resolve_rev(&self, repo: RepoId, rev: &str) -> FluxResult<ChangesetId> {
        let rev = rev.trim();
        if rev.is_empty() {
            return Err(FluxError::InvalidArgument("empty revision".into()));
        }
        let split = rev.find(['~', '^']).unwrap_or(rev.len());
        let (base, mut ops) = rev.split_at(split);
        let mut id = self.resolve_base(repo, base).await?;
        while !ops.is_empty() {
            let op = ops.as_bytes()[0];
            let rest = &ops[1..];
            let digits = rest.find(['~', '^']).unwrap_or(rest.len());
            let n: usize = if digits == 0 {
                1
            } else {
                rest[..digits].parse().map_err(|_| FluxError::InvalidArgument(format!("bad revision {rev:?}")))?
            };
            ops = &rest[digits..];
            if op == b'~' {
                for _ in 0..n {
                    let cs = self.load_one(repo, &id).await?;
                    id = *cs
                        .changeset
                        .parents
                        .first()
                        .ok_or_else(|| FluxError::NotFound(format!("{rev:?}: {id} has no parent")))?;
                }
            } else if n > 0 {
                let cs = self.load_one(repo, &id).await?;
                id = *cs
                    .changeset
                    .parents
                    .get(n - 1)
                    .ok_or_else(|| FluxError::NotFound(format!("{rev:?}: {id} has no parent #{n}")))?;
            }
        }
        Ok(id)
    }

    async fn resolve_base(&self, repo: RepoId, base: &str) -> FluxResult<ChangesetId> {
        if let Some(sha) = base.strip_prefix("git:") {
            let bytes: [u8; 20] = hex_decode(sha)
                .and_then(|b| b.try_into().ok())
                .ok_or_else(|| FluxError::InvalidArgument(format!("bad git sha {sha:?}")))?;
            return self
                .meta
                .git_to_cs(repo, &[bytes])
                .await?
                .pop()
                .flatten()
                .ok_or_else(|| FluxError::NotFound(format!("no changeset for git:{sha}")));
        }
        if let Some(r) = self.meta.get_ref(repo, base.as_bytes()).await? {
            return Ok(r.target);
        }
        // Git spellings: refs/heads/x, refs/tags/x, and bare tag names.
        let alt = match (base.strip_prefix("refs/heads/"), base.strip_prefix("refs/tags/")) {
            (Some(b), _) => Some(b.to_string()),
            (_, Some(t)) => Some(format!("tags/{t}")),
            _ => Some(format!("tags/{base}")),
        };
        if let Some(a) = alt
            && let Some(r) = self.meta.get_ref(repo, a.as_bytes()).await?
        {
            return Ok(r.target);
        }
        // A full git commit SHA-1 (imported or generated by the git frontend).
        if base.len() == 40
            && let Some(bytes) = hex_decode(base).and_then(|b| <[u8; 20]>::try_from(b).ok())
        {
            if let Some(cs) = self.meta.git_to_cs(repo, &[bytes]).await?.pop().flatten() {
                return Ok(cs);
            }
            if let Some(cs) = self.meta.git_commits_lookup(repo, &[bytes]).await?.pop().flatten() {
                return Ok(cs);
            }
        }
        let hex = base.strip_prefix("cs_").unwrap_or(base);
        if hex.len() >= 6 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            let found = self.meta.resolve_prefix(repo, hex, 2).await?;
            return match found.as_slice() {
                [one] => Ok(*one),
                [] => Err(FluxError::NotFound(format!("unknown revision {base:?}"))),
                _ => Err(FluxError::InvalidArgument(format!("ambiguous revision prefix {base:?}"))),
            };
        }
        Err(FluxError::NotFound(format!("unknown revision {base:?}")))
    }

    // ───────────── history ─────────────

    pub async fn entry_at(&self, repo: RepoId, id: &ChangesetId, path: &RepoPath) -> FluxResult<Option<TreeEntry>> {
        let root = self.root_tree(repo, id).await?;
        Ok(lookup_path(&self.blobs, root, path).await?)
    }

    // ───────────── refs ─────────────

    pub async fn update_ref(
        &self,
        repo: RepoId,
        name: &[u8],
        expect: RefExpect,
        new: Option<ChangesetId>,
        actor: &str,
        reason: String,
    ) -> FluxResult<(CasOutcome, Option<RefValue>)> {
        validate_ref_name(name)?;
        if let Some(target) = new {
            self.load_one(repo, &target).await?;
        }
        let old = self.meta.get_ref(repo, name).await?.map(|r| r.target);
        let outcome = self.meta.cas_ref(repo, name, expect, new, RefLogEntry { actor: actor.into(), reason }).await?;
        let current = match &outcome {
            CasOutcome::Mismatch { current } => *current,
            _ => None,
        };
        if !matches!(outcome, CasOutcome::Mismatch { .. }) {
            let repo_name = self.repo_name(repo).await;
            self.publish(Event::RefMoved { repo: repo_name, name: name.to_vec(), old, new, actor: actor.into() });
        }
        Ok((outcome, current))
    }
}

/// Ref names: non-empty, no revision operators, no whitespace or control chars.
pub fn validate_ref_name(name: &[u8]) -> FluxResult<()> {
    let ok = !name.is_empty()
        && name.len() <= 255
        && !name.starts_with(b"/")
        && !name.ends_with(b"/")
        && !name.windows(2).any(|w| w == b"//")
        && !name.starts_with(b"git:")
        && !name.starts_with(b"cs_")
        && name.iter().all(|b| b.is_ascii_graphic() && !b"~^:\\?*[".contains(b));
    if ok { Ok(()) } else { Err(FluxError::InvalidArgument(format!("invalid ref name {:?}", String::from_utf8_lossy(name)))) }
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect()
}
