//! Deterministic git objects for flux data.
//!
//! * blob: the file content (its git SHA-1 is stored in `content_meta`);
//! * tree: a flux tree re-sorted by git rules (directories compare as
//!   `name/`), with git modes;
//! * commit: built from the changeset's parents, author/committer, extra
//!   headers and message. Changesets imported from git carry the original
//!   header bytes, so regenerated commits hash to the original SHA-1s. A
//!   changeset may also carry the verbatim commit in `extra["git.commit"]`
//!   (used when a pushed commit can't be reproduced field by field).
//!
//! Mappings are memoized in memory and persisted (`git_trees` is global,
//! `git_commits` per repository), so every object is generated once.

use std::collections::HashMap;
use std::sync::Arc;

use flux_core::{ChangesetId, ContentId, EntryKind, FileKind, Tree, TreeId};
use flux_store::RepoId;
use sha1::Digest as _;

use crate::core::{Flux, FluxError, FluxResult};

pub type Sha = [u8; 20];

pub const EMPTY_TREE: Sha =
    [0x4b, 0x82, 0x5d, 0xc6, 0x42, 0xcb, 0x6e, 0xb9, 0xa0, 0x60, 0xe5, 0x4b, 0xf8, 0xd6, 0x92, 0x88, 0xfb, 0xee, 0x49, 0x04];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Commit = 1,
    Tree = 2,
    Blob = 3,
    Tag = 4,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Commit => "commit",
            Kind::Tree => "tree",
            Kind::Blob => "blob",
            Kind::Tag => "tag",
        }
    }
}

pub fn hash_object(kind: Kind, data: &[u8]) -> Sha {
    let mut h = sha1::Sha1::new();
    h.update(format!("{} {}\0", kind.name(), data.len()).as_bytes());
    h.update(data);
    h.finalize().into()
}

pub fn hex(sha: &Sha) -> String {
    hex::encode(sha)
}

pub fn parse_hex(s: &[u8]) -> Option<Sha> {
    let v = hex::decode(s).ok()?;
    v.try_into().ok()
}

/// One entry of a git tree as flux sees it.
#[derive(Debug, Clone)]
pub struct GitEntry {
    pub name: Vec<u8>,
    pub mode: u32,
    pub sha: Sha,
    /// The flux object behind it (for walking).
    pub target: Target,
}

#[derive(Debug, Clone, Copy)]
pub enum Target {
    Tree(TreeId),
    Blob(ContentId),
    Gitlink,
}

fn mode_of(kind: &EntryKind) -> u32 {
    match kind {
        EntryKind::Dir => 0o040000,
        EntryKind::File(FileKind::Regular) => 0o100644,
        EntryKind::File(FileKind::Executable) => 0o100755,
        EntryKind::File(FileKind::Symlink) => 0o120000,
        EntryKind::File(FileKind::GitLink(_)) => 0o160000,
        EntryKind::File(FileKind::GitMode(m)) => *m,
    }
}

/// Git's tree order: compare names as if directories ended in '/'.
pub fn git_sort(entries: &mut [GitEntry]) {
    entries.sort_by(|a, b| {
        let ka = a.name.iter().copied().chain((a.mode == 0o040000).then_some(b'/'));
        let kb = b.name.iter().copied().chain((b.mode == 0o040000).then_some(b'/'));
        ka.cmp(kb)
    });
}

pub fn encode_tree(entries: &[GitEntry]) -> Vec<u8> {
    let mut out = Vec::with_capacity(entries.len() * 40);
    for e in entries {
        out.extend_from_slice(format!("{:o} ", e.mode).as_bytes());
        out.extend_from_slice(&e.name);
        out.push(0);
        out.extend_from_slice(&e.sha);
    }
    out
}

fn tz(min: i16) -> String {
    let sign = if min < 0 { '-' } else { '+' };
    let m = (min as i32).abs();
    format!("{sign}{:02}{:02}", m / 60, m % 60)
}

/// The git commit bytes for a changeset, given its tree and parents' SHAs.
pub fn encode_commit(cs: &flux_core::Changeset, tree: &Sha, parents: &[Sha]) -> Vec<u8> {
    if let Some(raw) = cs.extra.get("git.commit") {
        return raw.clone();
    }
    let mut out = Vec::with_capacity(256 + cs.message.len());
    out.extend_from_slice(b"tree ");
    out.extend_from_slice(hex(tree).as_bytes());
    out.push(b'\n');
    for p in parents {
        out.extend_from_slice(b"parent ");
        out.extend_from_slice(hex(p).as_bytes());
        out.push(b'\n');
    }
    match (cs.extra.get("git.raw_author"), cs.extra.get("git.raw_committer")) {
        (Some(a), Some(c)) => {
            out.extend_from_slice(b"author ");
            out.extend_from_slice(a);
            out.push(b'\n');
            out.extend_from_slice(b"committer ");
            out.extend_from_slice(c);
            out.push(b'\n');
        }
        _ => {
            let ident = |who: &[u8], t: &flux_core::Timestamp| {
                let mut v = who.to_vec();
                v.extend_from_slice(format!(" {} {}", t.unix_ms.div_euclid(1000), tz(t.tz_offset_min)).as_bytes());
                v
            };
            out.extend_from_slice(b"author ");
            out.extend_from_slice(&ident(&cs.author, &cs.author_time));
            out.push(b'\n');
            out.extend_from_slice(b"committer ");
            match (&cs.committer, &cs.committer_time) {
                (Some(c), Some(t)) => out.extend_from_slice(&ident(c, t)),
                _ => out.extend_from_slice(&ident(&cs.author, &cs.author_time)),
            }
            out.push(b'\n');
        }
    }
    if let Some(extra) = cs.extra.get("git.extra_headers") {
        out.extend_from_slice(extra);
    }
    out.push(b'\n');
    out.extend_from_slice(&cs.message);
    out
}

/// Per-process caches for generated objects.
pub struct GitCaches {
    pub trees: moka::sync::Cache<TreeId, Sha>,
    pub blobs: moka::sync::Cache<ContentId, Sha>,
    pub commits: moka::sync::Cache<(RepoId, ChangesetId), Sha>,
    /// Tree mappings not yet persisted (written in batches).
    pending_trees: std::sync::Mutex<Vec<(TreeId, Sha)>>,
}

impl Default for GitCaches {
    fn default() -> Self {
        Self::new()
    }
}

impl GitCaches {
    pub fn new() -> Self {
        Self {
            trees: moka::sync::Cache::new(4_000_000),
            blobs: moka::sync::Cache::new(4_000_000),
            commits: moka::sync::Cache::new(4_000_000),
            pending_trees: Default::default(),
        }
    }
}

impl Flux {
    /// Git SHA-1s of contents (from content metadata).
    pub async fn git_blob_shas(&self, ids: &[ContentId]) -> FluxResult<Vec<Sha>> {
        let mut out = vec![[0u8; 20]; ids.len()];
        let mut missing = Vec::new();
        for (i, id) in ids.iter().enumerate() {
            match self.git.blobs.get(id) {
                Some(s) => out[i] = s,
                None => missing.push(i),
            }
        }
        if !missing.is_empty() {
            let want: Vec<ContentId> = missing.iter().map(|i| ids[*i]).collect();
            let metas = self.meta.get_content_meta(&want).await?;
            for (i, m) in missing.into_iter().zip(metas) {
                let m = m.ok_or_else(|| FluxError::Internal(anyhow::anyhow!("content {} has no metadata", ids[i])))?;
                self.git.blobs.insert(ids[i], m.git_sha1);
                out[i] = m.git_sha1;
            }
        }
        Ok(out)
    }

    /// The git entries of a flux tree (child tree SHAs computed as needed).
    pub async fn git_tree_entries(&self, tree: &Tree) -> FluxResult<Vec<GitEntry>> {
        let contents: Vec<ContentId> = tree.entries().iter().filter_map(|e| e.content_id()).collect();
        let blob_shas: HashMap<ContentId, Sha> = contents.iter().copied().zip(self.git_blob_shas(&contents).await?).collect();
        let mut out = Vec::with_capacity(tree.entries().len());
        for e in tree.entries() {
            let (sha, target) = match e.kind {
                EntryKind::Dir => {
                    let t = e.tree_id().unwrap();
                    (Box::pin(self.git_tree_sha_buffered(t)).await?, Target::Tree(t))
                }
                EntryKind::File(FileKind::GitLink(s)) => (s, Target::Gitlink),
                EntryKind::File(_) => {
                    let c = e.content_id().unwrap();
                    (blob_shas[&c], Target::Blob(c))
                }
            };
            out.push(GitEntry { name: e.name.as_bytes().to_vec(), mode: mode_of(&e.kind), sha, target });
        }
        git_sort(&mut out);
        Ok(out)
    }

    /// The git tree SHA-1 of a flux tree (memoized and persisted).
    pub async fn git_tree_sha(&self, id: TreeId) -> FluxResult<Sha> {
        let sha = self.git_tree_sha_buffered(id).await?;
        self.flush_git_trees().await?;
        Ok(sha)
    }

    /// Persist buffered tree mappings.
    pub async fn flush_git_trees(&self) -> FluxResult<()> {
        let batch = std::mem::take(&mut *self.git.pending_trees.lock().unwrap());
        if !batch.is_empty() {
            self.meta.git_trees_put(&batch).await?;
        }
        Ok(())
    }

    /// The flux tree for a git tree SHA-1 generated earlier.
    pub async fn git_tree_by_sha(&self, sha: &Sha) -> FluxResult<Option<TreeId>> {
        self.flush_git_trees().await?;
        Ok(self.meta.git_tree_by_sha(sha).await?)
    }

    /// Like `git_tree_sha`, but new mappings are only buffered (flushed in
    /// batches) so generating a long history costs no write per tree.
    async fn git_tree_sha_buffered(&self, id: TreeId) -> FluxResult<Sha> {
        if let Some(s) = self.git.trees.get(&id) {
            return Ok(s);
        }
        if let Some(Some(s)) = self.meta.git_trees_get(std::slice::from_ref(&id)).await?.pop() {
            self.git.trees.insert(id, s);
            return Ok(s);
        }
        let tree = self.blobs.get_tree(&id).await?.ok_or_else(|| FluxError::Internal(anyhow::anyhow!("tree {id} missing")))?;
        let bytes = encode_tree(&self.git_tree_entries(&tree).await?);
        let sha = hash_object(Kind::Tree, &bytes);
        self.git.trees.insert(id, sha);
        let full = {
            let mut p = self.git.pending_trees.lock().unwrap();
            p.push((id, sha));
            p.len() >= 2000
        };
        if full {
            self.flush_git_trees().await?;
        }
        Ok(sha)
    }

    pub async fn git_tree_bytes(&self, id: TreeId) -> FluxResult<(Vec<u8>, Vec<GitEntry>)> {
        let tree = self.blobs.get_tree(&id).await?.ok_or_else(|| FluxError::Internal(anyhow::anyhow!("tree {id} missing")))?;
        let entries = self.git_tree_entries(&tree).await?;
        Ok((encode_tree(&entries), entries))
    }

    /// The git commit for a changeset: `(sha, bytes, tree sha)`. Ancestors
    /// without a mapping are generated first (iteratively, oldest first).
    pub async fn git_commit(&self, repo: RepoId, cs: &ChangesetId) -> FluxResult<(Sha, Vec<u8>, Sha)> {
        self.git_commit_shas(repo, std::slice::from_ref(cs)).await?;
        let l = self.load_one_arc(repo, cs).await?;
        let tree = self.git_tree_sha(self.root_tree(repo, cs).await?).await?;
        let parents = self.git_commit_shas(repo, &l.changeset.parents).await?;
        let bytes = encode_commit(&l.changeset, &tree, &parents);
        Ok((hash_object(Kind::Commit, &bytes), bytes, tree))
    }

    /// Git commit SHA-1s for changesets, generating (and persisting) any
    /// missing mappings, ancestors first.
    pub async fn git_commit_shas(&self, repo: RepoId, ids: &[ChangesetId]) -> FluxResult<Vec<Sha>> {
        let mut known: HashMap<ChangesetId, Sha> = HashMap::new();
        let mut missing: Vec<ChangesetId> = Vec::new();
        for id in ids {
            match self.git.commits.get(&(repo, *id)) {
                Some(s) => {
                    known.insert(*id, s);
                }
                None => missing.push(*id),
            }
        }
        if !missing.is_empty() {
            for (id, s) in missing.iter().zip(self.meta.git_commits_of(repo, &missing).await?) {
                if let Some(s) = s {
                    self.git.commits.insert((repo, *id), s);
                    known.insert(*id, s);
                }
            }
            let todo: Vec<ChangesetId> = missing.into_iter().filter(|id| !known.contains_key(id)).collect();
            if !todo.is_empty() {
                self.generate_commits(repo, &todo, &mut known).await?;
            }
        }
        ids.iter()
            .map(|id| known.get(id).copied().ok_or_else(|| FluxError::Internal(anyhow::anyhow!("no git commit for {id}"))))
            .collect()
    }

    async fn generate_commits(
        &self,
        repo: RepoId,
        roots: &[ChangesetId],
        known: &mut HashMap<ChangesetId, Sha>,
    ) -> FluxResult<()> {
        // Collect unmapped ancestors (graph order is topological).
        let g = self.repo_graph(repo).await?;
        let mut need: Vec<u32> = Vec::new();
        let mut stack: Vec<u32> = Vec::new();
        for r in roots {
            stack.push(self.graph_index(repo, r).await?.1);
        }
        let mut seen = std::collections::HashSet::new();
        while let Some(i) = stack.pop() {
            if !seen.insert(i) {
                continue;
            }
            let (id, parents): (ChangesetId, Vec<u32>) = {
                let graph = g.graph.read().unwrap();
                (graph.id(i), graph.parents(i).to_vec())
            };
            if known.contains_key(&id) || self.git.commits.get(&(repo, id)).is_some() {
                continue;
            }
            need.push(i);
            stack.extend(parents);
        }
        // Drop commits whose mapping is already persisted (their ancestors
        // were collected too, which is harmless: they are skipped here).
        need.sort_unstable();
        let ids: Vec<ChangesetId> = {
            let graph = g.graph.read().unwrap();
            need.iter().map(|i| graph.id(*i)).collect()
        };
        let mut todo = Vec::with_capacity(ids.len());
        for chunk in ids.chunks(4000) {
            for (id, s) in chunk.iter().zip(self.meta.git_commits_of(repo, chunk).await?) {
                match s {
                    Some(s) => {
                        self.git.commits.insert((repo, *id), s);
                        known.insert(*id, s);
                    }
                    None => todo.push(*id),
                }
            }
        }
        let mut batch: Vec<(ChangesetId, Sha)> = Vec::new();
        for chunk in todo.chunks(512) {
            // Trees are independent of each other: compute them concurrently.
            let trees: Vec<(Arc<crate::core::Loaded>, Sha)> = {
                use futures::StreamExt as _;
                let futs: Vec<_> = chunk
                    .iter()
                    .map(|id| async move {
                        let l = self.load_one_arc(repo, id).await?;
                        let tree = self.git_tree_sha_buffered(self.root_tree(repo, id).await?).await?;
                        FluxResult::Ok((l, tree))
                    })
                    .collect();
                futures::stream::iter(futs).buffered(16).collect::<Vec<_>>().await.into_iter().collect::<FluxResult<_>>()?
            };
            for (id, (l, tree)) in chunk.iter().zip(trees) {
                let id = *id;
                let mut parents = Vec::with_capacity(l.changeset.parents.len());
                for p in &l.changeset.parents {
                    let s = match known.get(p).copied().or_else(|| self.git.commits.get(&(repo, *p))) {
                        Some(s) => s,
                        None => self
                            .meta
                            .git_commits_of(repo, &[*p])
                            .await?
                            .pop()
                            .flatten()
                            .ok_or_else(|| FluxError::Internal(anyhow::anyhow!("parent {p} has no git commit")))?,
                    };
                    parents.push(s);
                }
                let sha = hash_object(Kind::Commit, &encode_commit(&l.changeset, &tree, &parents));
                known.insert(id, sha);
                self.git.commits.insert((repo, id), sha);
                batch.push((id, sha));
            }
            // Trees before commits: a persisted commit implies its trees are.
            self.flush_git_trees().await?;
            self.meta.git_commits_put(repo, &batch).await?;
            batch.clear();
        }
        if !batch.is_empty() {
            self.meta.git_commits_put(repo, &batch).await?;
        }
        Ok(())
    }

    /// Changeset for a generated git commit SHA-1.
    pub async fn git_lookup_commit(&self, repo: RepoId, sha: &Sha) -> FluxResult<Option<ChangesetId>> {
        Ok(self.meta.git_commits_lookup(repo, std::slice::from_ref(sha)).await?.pop().flatten())
    }

    pub fn git_caches(&self) -> &Arc<GitCaches> {
        &self.git
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_tree_sha() {
        assert_eq!(hash_object(Kind::Tree, &[]), EMPTY_TREE);
    }

    #[test]
    fn git_order_puts_dirs_after_dotted_files() {
        let e = |n: &str, dir: bool| GitEntry {
            name: n.as_bytes().to_vec(),
            mode: if dir { 0o040000 } else { 0o100644 },
            sha: [0; 20],
            target: Target::Gitlink,
        };
        let mut v = vec![e("foo", true), e("foo.c", false), e("foo-bar", false)];
        git_sort(&mut v);
        let names: Vec<&[u8]> = v.iter().map(|x| x.name.as_slice()).collect();
        assert_eq!(names, vec![&b"foo-bar"[..], b"foo.c", b"foo"]);
    }

    #[test]
    fn tz_format() {
        assert_eq!(tz(330), "+0530");
        assert_eq!(tz(-480), "-0800");
        assert_eq!(tz(0), "+0000");
    }
}
