//! Server-side operations, independent of the RPC layer (SPEC §7).

use std::collections::{BinaryHeap, HashSet};
use std::sync::Arc;

use flux_core::{Changeset, ChangesetId, ContentId, FileChange, FileKind, RepoPath, TreeEntry, TreeId};
use flux_derive::{DeriveError, TREE_V1, TreeWriter, derive_tree, derive_tree_buffered, diff_trees, lookup_path};
use flux_proto::v1;
use flux_store::{
    BlobStore, CasOutcome, ContentMeta, MetaStore, NewChangeset, RefExpect, RefLogEntry, RefValue, RepoId, StoreError,
};
use thiserror::Error;
use tonic::Status;

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
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl From<StoreError> for FluxError {
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::RepoNotFound(r) => FluxError::NotFound(format!("repository {r:?} not found")),
            StoreError::RepoExists(r) => FluxError::AlreadyExists(format!("repository {r:?} already exists")),
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

pub struct Flux {
    pub meta: Arc<dyn MetaStore>,
    pub blobs: BlobStore,
    repos: moka::future::Cache<String, RepoId>,
}

impl Flux {
    pub fn new(meta: Arc<dyn MetaStore>, blobs: BlobStore) -> Self {
        Self { meta, blobs, repos: moka::future::Cache::new(10_000) }
    }

    pub async fn repo(&self, name: &str) -> FluxResult<RepoId> {
        if let Some(id) = self.repos.get(name).await {
            return Ok(id);
        }
        let info = self.meta.get_repo(name).await?.ok_or_else(|| FluxError::NotFound(format!("repository {name:?} not found")))?;
        self.repos.insert(name.to_string(), info.id).await;
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
    pub async fn put_contents(
        &self,
        items: Vec<(bytes::Bytes, Option<ContentId>)>,
    ) -> FluxResult<Vec<(ContentId, ContentMeta)>> {
        let out = self.blobs.put_contents(items).await?;
        self.blobs.sync().await?;
        self.meta.put_content_meta(&out).await?;
        Ok(out)
    }

    // ───────────── changesets ─────────────

    pub async fn load(&self, repo: RepoId, ids: &[ChangesetId]) -> FluxResult<Vec<Option<Loaded>>> {
        let stored = self.meta.get_changesets(repo, ids).await?;
        stored
            .into_iter()
            .map(|s| {
                s.map(|s| {
                    let changeset = Changeset::decode_verified(&s.body, &s.id)
                        .map_err(|e| FluxError::Internal(anyhow::anyhow!("corrupt changeset {}: {e}", s.id)))?;
                    Ok(Loaded { id: s.id, gen_num: s.gen_num, log_seq: s.log_seq, canonical: s.body, changeset })
                })
                .transpose()
            })
            .collect()
    }

    pub async fn load_one(&self, repo: RepoId, id: &ChangesetId) -> FluxResult<Loaded> {
        self.load(repo, std::slice::from_ref(id))
            .await?
            .pop()
            .flatten()
            .ok_or_else(|| FluxError::NotFound(format!("changeset {id} not found")))
    }

    /// Root tree of a changeset, deriving it (and any underived ancestors) if
    /// the mapping is missing, e.g. after a crash between insert and derive.
    pub async fn root_tree(&self, repo: RepoId, id: &ChangesetId) -> FluxResult<TreeId> {
        if let Some(Some(v)) = self.meta.get_derived(repo, TREE_V1, std::slice::from_ref(id)).await?.pop() {
            return TreeId::from_slice(&v).map_err(|e| FluxError::Internal(e.into()));
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
        if batch.is_empty() {
            return Ok(vec![]);
        }
        if batch.len() > 10_000 {
            return Err(FluxError::InvalidArgument("at most 10000 changesets per batch".into()));
        }
        let t0 = std::time::Instant::now();
        let mut phases: Vec<(&str, u128)> = Vec::new();
        let mark = |name: &'static str, phases: &mut Vec<(&str, u128)>| phases.push((name, t0.elapsed().as_micros()));
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

        // External parents must exist.
        let external: Vec<ChangesetId> = batch
            .iter()
            .flat_map(|c| c.parents.iter().copied())
            .filter(|p| !in_batch.contains_key(p))
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        let mut gens: std::collections::HashMap<ChangesetId, u64> = std::collections::HashMap::new();
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
        let mut trees: std::collections::HashMap<ChangesetId, TreeId> = std::collections::HashMap::new();
        let mut rows = Vec::with_capacity(batch.len());
        let mut out_trees = Vec::with_capacity(batch.len());
        let mut bodies = Vec::with_capacity(batch.len());
        for (cs, id) in batch.iter().zip(&ids) {
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
            let body = cs.encode();
            rows.push(NewChangeset { id: *id, parents: cs.parents.clone(), gen_num, body: body.clone() });
            out_trees.push(tree);
            bodies.push(body);
        }
        mark("derive", &mut phases);
        writer.flush().await?;
        mark("flush", &mut phases);
        // Checks that need parent trees run after the flush so in-batch
        // parents are readable. Rejected batches leave only unreferenced
        // (content-addressed) trees behind.
        {
            // Merge checks are CPU-heavy tree diffs: run them on all cores.
            let trees = Arc::new(trees.clone());
            let mut tasks = tokio::task::JoinSet::new();
            for (cs, id) in batch.iter().zip(&ids) {
                let needs = cs.parents.len() > 1
                    || cs.file_changes.values().any(|c| matches!(c, FileChange::Change { copy_from: Some(_), .. }));
                if !needs {
                    continue;
                }
                let (this, cs, id, trees) = (self.clone(), cs.clone(), *id, trees.clone());
                tasks.spawn(async move { this.check_against_parents(repo, &id, &cs, &trees).await });
            }
            while let Some(r) = tasks.join_next().await {
                r.map_err(|e| FluxError::Internal(e.into()))??;
            }
        }
        mark("check", &mut phases);
        self.blobs.sync().await?;
        mark("sync", &mut phases);

        let seqs = self.meta.put_changesets(repo, &rows).await?;
        mark("insert", &mut phases);
        let derived: Vec<(ChangesetId, Vec<u8>)> = ids.iter().zip(&out_trees).map(|(i, t)| (*i, t.as_bytes().to_vec())).collect();
        self.meta.put_derived(repo, TREE_V1, &derived).await?;
        mark("derived", &mut phases);
        tracing::debug!(n = batch.len(), ?phases, "create_changesets");

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
    ) -> FluxResult<()> {
        let needs_copy_check =
            cs.file_changes.values().any(|c| matches!(c, FileChange::Change { copy_from: Some(_), .. }));
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
            if let FileChange::Change { copy_from: Some((src, idx)), .. } = change {
                let found = lookup_path(&self.blobs, parent_trees[*idx as usize], src).await?;
                if found.is_none_or(|e| e.is_dir()) {
                    return Err(FluxError::InvalidArgument(format!(
                        "{path}: copy_from {src} does not exist in parent #{idx}"
                    )));
                }
            }
        }
        let all = |_: &RepoPath, _: bool| true;
        for pt in &parent_trees[1..] {
            for d in diff_trees(&self.blobs, Some(parent_trees[0]), Some(*pt), &all).await? {
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
                    id = *cs.changeset.parents.first().ok_or_else(|| {
                        FluxError::NotFound(format!("{rev:?}: {id} has no parent"))
                    })?;
                }
            } else if n > 0 {
                let cs = self.load_one(repo, &id).await?;
                id = *cs.changeset.parents.get(n - 1).ok_or_else(|| {
                    FluxError::NotFound(format!("{rev:?}: {id} has no parent #{n}"))
                })?;
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

    /// Ancestors of `start` in descending generation order.
    pub async fn log(
        &self,
        repo: RepoId,
        start: ChangesetId,
        limit: usize,
        first_parent: bool,
        path: Option<&RepoPath>,
        mut emit: impl FnMut(Loaded) -> bool,
    ) -> FluxResult<()> {
        let mut heap: BinaryHeap<(u64, ChangesetId)> = BinaryHeap::new();
        let mut seen: HashSet<ChangesetId> = HashSet::new();
        let first = self.load_one(repo, &start).await?;
        heap.push((first.gen_num, start));
        seen.insert(start);
        let mut emitted = 0;
        while let Some((_, id)) = heap.pop() {
            let cs = self.load_one(repo, &id).await?;
            let parents: Vec<ChangesetId> =
                if first_parent { cs.changeset.parents.iter().take(1).copied().collect() } else { cs.changeset.parents.clone() };
            let new: Vec<ChangesetId> = parents.into_iter().filter(|p| seen.insert(*p)).collect();
            if !new.is_empty() {
                for (p, g) in new.iter().zip(self.meta.get_gens(repo, &new).await?) {
                    heap.push((g.unwrap_or(0), *p));
                }
            }
            let touches = match path {
                None => true,
                Some(path) => self.touches_path(repo, &cs, path).await?,
            };
            if touches {
                emitted += 1;
                if !emit(cs) || emitted >= limit {
                    break;
                }
            }
        }
        Ok(())
    }

    async fn touches_path(&self, repo: RepoId, cs: &Loaded, path: &RepoPath) -> FluxResult<bool> {
        if cs.changeset.parents.len() <= 1 {
            // Non-merge: the listed changes are exactly the first-parent diff.
            // (A listed no-op deletion can over-report; that's acceptable.)
            return Ok(cs.changeset.file_changes.keys().any(|p| p.starts_with(path) || path.starts_with(p)));
        }
        // Merges list paths where parents disagree even if unchanged relative
        // to the first parent, so compare against the first parent's state.
        let here = self.entry_at(repo, &cs.id, path).await?;
        let there = self.entry_at(repo, &cs.changeset.parents[0], path).await?;
        Ok(here != there)
    }

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
        reason: String,
    ) -> FluxResult<(CasOutcome, Option<RefValue>)> {
        validate_ref_name(name)?;
        if let Some(target) = new {
            self.load_one(repo, &target).await?;
        }
        let outcome = self.meta.cas_ref(repo, name, expect, new, RefLogEntry { actor: "anonymous".into(), reason }).await?;
        let current = match &outcome {
            CasOutcome::Mismatch { current } => *current,
            _ => None,
        };
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
