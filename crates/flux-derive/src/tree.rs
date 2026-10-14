//! `tree_v1` derivation (SPEC §6.3) and tree traversal helpers.
//!
//! Derivation rewrites only the subtrees touched by a changeset; untouched
//! subtree ids are reused, so cost is O(changes × depth), independent of repo
//! size.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use flux_core::{ContentId, EntryKind, FileChange, FileKind, PathComponent, RepoPath, Tree, TreeEntry, TreeId};
use flux_store::BlobStore;

use crate::{DeriveError, DeriveResult};

type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Default)]
struct Node<'a> {
    leaf: Option<&'a FileChange>,
    children: BTreeMap<&'a [u8], Node<'a>>,
}

fn build_trie(changes: &BTreeMap<RepoPath, FileChange>) -> Node<'_> {
    let mut root = Node::default();
    for (path, change) in changes {
        // Retain keeps parents[0]'s value: nothing to apply.
        if matches!(change, FileChange::Retain) {
            continue;
        }
        let mut node = &mut root;
        for comp in path.components() {
            node = node.children.entry(comp).or_default();
        }
        node.leaf = Some(change);
    }
    root
}

async fn load(blobs: &BlobStore, id: TreeId) -> DeriveResult<Arc<Tree>> {
    blobs.get_tree(&id).await?.ok_or(DeriveError::MissingTree(id))
}

/// Buffers newly derived trees in memory so a batch of derivations can be
/// written to the blob store concurrently. Callers MUST [`TreeWriter::flush`]
/// before persisting anything that references the trees (SPEC §5.2 ordering).
pub struct TreeWriter<'a> {
    blobs: &'a BlobStore,
    /// New trees (with canonical bytes) and the previous version of the same
    /// directory as a delta hint.
    pending: std::sync::Mutex<std::collections::HashMap<TreeId, (Arc<Tree>, bytes::Bytes, Option<TreeId>)>>,
}

impl<'a> TreeWriter<'a> {
    pub fn new(blobs: &'a BlobStore) -> Self {
        Self { blobs, pending: Default::default() }
    }

    pub async fn get(&self, id: TreeId) -> DeriveResult<Arc<Tree>> {
        if let Some((t, _, _)) = self.pending.lock().unwrap().get(&id) {
            return Ok(t.clone());
        }
        load(self.blobs, id).await
    }

    /// Buffer a new tree. `base` is the previous version of the same
    /// directory, used as a delta hint by the blob store.
    pub fn put(&self, tree: Tree, base: Option<TreeId>) -> TreeId {
        let bytes = bytes::Bytes::from(tree.encode());
        let id = TreeId::hash(&bytes);
        self.pending.lock().unwrap().entry(id).or_insert_with(|| (Arc::new(tree), bytes, base.filter(|b| *b != id)));
        id
    }

    /// Write all pending trees in one batch and clear the buffer. Callers
    /// must still `BlobStore::sync` before committing references to them.
    pub async fn flush(&self) -> DeriveResult<usize> {
        let trees: Vec<_> = self.pending.lock().unwrap().drain().map(|(id, (t, b, base))| (id, t, b, base)).collect();
        let n = trees.len();
        self.blobs.put_trees_encoded(trees).await?;
        Ok(n)
    }
}

/// Apply `node` to the directory `base`. Returns `None` if the result is empty.
///
/// The base entries and the trie's children are both sorted by name, so the
/// new directory is built in one merge pass; subdirectories are derived
/// concurrently.
fn apply<'a>(
    blobs: &'a TreeWriter<'a>,
    base: Option<TreeId>,
    node: &'a Node<'a>,
) -> BoxFut<'a, DeriveResult<Option<(TreeId, u64, u64)>>> {
    Box::pin(async move {
        let base_tree = match base {
            Some(id) => Some(blobs.get(id).await?),
            None => None,
        };
        let old: &[TreeEntry] = base_tree.as_deref().map(Tree::entries).unwrap_or(&[]);
        let find = |name: &[u8]| old.binary_search_by(|e| e.name.as_bytes().cmp(name)).ok().map(|i| &old[i]);

        // Recurse into changed subdirectories concurrently.
        let subdirs: Vec<(&[u8], &Node<'a>)> = node
            .children
            .iter()
            .filter(|(_, c)| !matches!(c.leaf, Some(FileChange::Change { .. })) && !c.children.is_empty())
            .map(|(n, c)| (*n, c))
            .collect();
        let results = futures::future::try_join_all(subdirs.iter().map(|(name, child)| {
            // A file being replaced by a directory starts from an empty subtree.
            let sub_base = find(name).and_then(|e| e.tree_id());
            apply(blobs, sub_base, child)
        }))
        .await?;
        let mut sub_results = subdirs.iter().map(|(n, _)| *n).zip(results);

        let mut out: Vec<TreeEntry> = Vec::with_capacity(old.len() + node.children.len());
        let mut oi = 0;
        for (name, child) in &node.children {
            while oi < old.len() && old[oi].name.as_bytes() < *name {
                out.push(old[oi].clone());
                oi += 1;
            }
            let existing = if oi < old.len() && old[oi].name.as_bytes() == *name {
                oi += 1;
                Some(&old[oi - 1])
            } else {
                None
            };
            if let Some(FileChange::Change { content_id, size, kind, .. }) = child.leaf {
                // A file replaces whatever was there (file or directory); any
                // listed deletions underneath it are implied.
                let comp = match existing {
                    Some(e) => e.name.clone(),
                    None => PathComponent::new(*name)?,
                };
                out.push(TreeEntry::file(comp, *kind, *content_id, *size));
                continue;
            }
            if child.children.is_empty() {
                // Plain deletion. Deleting a directory path is a no-op.
                if let Some(e) = existing
                    && e.is_dir()
                {
                    out.push(e.clone());
                }
                continue;
            }
            let (_, result) = sub_results.next().expect("one result per subdirectory");
            match result {
                Some((id, size, files)) => {
                    let comp = match existing {
                        Some(e) => e.name.clone(),
                        None => PathComponent::new(*name)?,
                    };
                    out.push(TreeEntry::dir(comp, id, size, files));
                }
                // Nothing was created underneath: a file that wasn't itself
                // deleted survives (deleting `c/x` under file `c` is a no-op).
                None => {
                    if let Some(e) = existing
                        && !e.is_dir()
                        && child.leaf.is_none()
                    {
                        out.push(e.clone());
                    }
                }
            }
        }
        out.extend_from_slice(&old[oi..]);

        if out.is_empty() {
            return Ok(None);
        }
        let tree = Tree::from_sorted(out)?;
        let (size, files) = (tree.total_size(), tree.total_files());
        let id = blobs.put(tree, base);
        Ok(Some((id, size, files)))
    })
}

/// Derive a new root tree from `base` (the first parent's root, or `None` for
/// a root changeset) and a changeset's file changes.
pub async fn derive_tree(
    blobs: &BlobStore,
    base: Option<TreeId>,
    changes: &BTreeMap<RepoPath, FileChange>,
) -> DeriveResult<TreeId> {
    let writer = TreeWriter::new(blobs);
    let id = derive_tree_buffered(&writer, base, changes).await?;
    writer.flush().await?;
    blobs.sync().await?;
    Ok(id)
}

/// Like [`derive_tree`] but buffers new trees in `writer` (no I/O for writes).
pub async fn derive_tree_buffered<'a>(
    writer: &'a TreeWriter<'a>,
    base: Option<TreeId>,
    changes: &'a BTreeMap<RepoPath, FileChange>,
) -> DeriveResult<TreeId> {
    if changes.values().all(|c| matches!(c, FileChange::Retain))
        && let Some(b) = base
    {
        return Ok(b);
    }
    let trie = build_trie(changes);
    match apply(writer, base, &trie).await? {
        Some((id, _, _)) => Ok(id),
        None => Ok(writer.put(Tree::empty(), None)),
    }
}

/// A file found while walking a tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeFile {
    pub path: RepoPath,
    pub kind: FileKind,
    /// `None` for gitlinks.
    pub content_id: Option<ContentId>,
    pub size: u64,
}

impl TreeFile {
    fn from_entry(path: RepoPath, e: &TreeEntry) -> Option<Self> {
        match e.kind {
            EntryKind::File(kind) => Some(TreeFile { path, kind, content_id: e.content_id(), size: e.size }),
            EntryKind::Dir => None,
        }
    }
}

fn child_path(prefix: Option<&RepoPath>, name: &PathComponent) -> DeriveResult<RepoPath> {
    Ok(match prefix {
        Some(p) => p.join(name.as_bytes())?,
        None => RepoPath::new(name.as_bytes())?,
    })
}

/// Walk all files under `root`. `include` limits descent to paths for which
/// the predicate returns true (it's called for directories and files).
pub async fn walk_tree(
    blobs: &BlobStore,
    root: TreeId,
    include: &(dyn Fn(&RepoPath, bool) -> bool + Sync),
) -> DeriveResult<Vec<TreeFile>> {
    let mut out = Vec::new();
    let mut stack: Vec<(Option<RepoPath>, TreeId)> = vec![(None, root)];
    while let Some((prefix, id)) = stack.pop() {
        let tree = load(blobs, id).await?;
        for e in tree.entries() {
            let path = child_path(prefix.as_ref(), &e.name)?;
            if !include(&path, e.is_dir()) {
                continue;
            }
            match e.tree_id() {
                Some(sub) => stack.push((Some(path), sub)),
                None => out.extend(TreeFile::from_entry(path, e)),
            }
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// Look up the entry at `path` under `root`.
pub async fn lookup_path(blobs: &BlobStore, root: TreeId, path: &RepoPath) -> DeriveResult<Option<TreeEntry>> {
    let mut current = root;
    let comps: Vec<&[u8]> = path.components().collect();
    for (i, comp) in comps.iter().enumerate() {
        let tree = load(blobs, current).await?;
        let Some(e) = tree.get(comp) else {
            return Ok(None);
        };
        if i == comps.len() - 1 {
            return Ok(Some(e.clone()));
        }
        match e.tree_id() {
            Some(sub) => current = sub,
            None => return Ok(None),
        }
    }
    Ok(None)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeDiffEntry {
    pub path: RepoPath,
    pub old: Option<TreeFile>,
    pub new: Option<TreeFile>,
}

/// File-level diff between two trees, pruning identical subtrees by id.
///
/// Directory pairs are merge-joined over their sorted entries; paths are only
/// materialized for entries that differ, so the cost is proportional to the
/// size of the difference.
pub async fn diff_trees(
    blobs: &BlobStore,
    old: Option<TreeId>,
    new: Option<TreeId>,
    include: &(dyn Fn(&RepoPath, bool) -> bool + Sync),
) -> DeriveResult<Vec<TreeDiffEntry>> {
    let mut out = Vec::new();
    let mut stack: Vec<(Option<RepoPath>, Option<TreeId>, Option<TreeId>)> = vec![(None, old, new)];
    let empty = Arc::new(Tree::empty());
    while let Some((prefix, a, b)) = stack.pop() {
        if a == b {
            continue;
        }
        let (ta, tb) = futures::future::try_join(
            async {
                match a {
                    Some(id) => load(blobs, id).await,
                    None => Ok(empty.clone()),
                }
            },
            async {
                match b {
                    Some(id) => load(blobs, id).await,
                    None => Ok(empty.clone()),
                }
            },
        )
        .await?;
        let (ea, eb) = (ta.entries(), tb.entries());
        let (mut i, mut j) = (0, 0);
        while i < ea.len() || j < eb.len() {
            let (x, y) = match (ea.get(i), eb.get(j)) {
                (Some(x), Some(y)) => match x.name.cmp(&y.name) {
                    std::cmp::Ordering::Less => (Some(x), None),
                    std::cmp::Ordering::Greater => (None, Some(y)),
                    std::cmp::Ordering::Equal => (Some(x), Some(y)),
                },
                // One side is exhausted.
                (x, y) => (x, y),
            };
            if x.is_some() {
                i += 1;
            }
            if y.is_some() {
                j += 1;
            }
            if let (Some(x), Some(y)) = (x, y)
                && x == y
            {
                continue;
            }
            let name = &x.or(y).unwrap().name;
            let path = child_path(prefix.as_ref(), name)?;
            let any_dir = x.is_some_and(|e| e.is_dir()) || y.is_some_and(|e| e.is_dir());
            if !include(&path, any_dir) && !include(&path, false) {
                continue;
            }
            let da = x.and_then(|e| e.tree_id());
            let db = y.and_then(|e| e.tree_id());
            let fa = x.and_then(|e| TreeFile::from_entry(path.clone(), e));
            let fb = y.and_then(|e| TreeFile::from_entry(path.clone(), e));
            if (fa.is_some() || fb.is_some()) && fa != fb && include(&path, false) {
                out.push(TreeDiffEntry { path: path.clone(), old: fa, new: fb });
            }
            if (da.is_some() || db.is_some()) && include(&path, true) {
                stack.push((Some(path), da, db));
            }
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn change(content: &[u8], kind: FileKind) -> FileChange {
        FileChange::Change { content_id: ContentId::hash(content), size: content.len() as u64, kind, copy_from: None }
    }

    fn p(s: &str) -> RepoPath {
        RepoPath::try_from(s).unwrap()
    }

    fn all(_: &RepoPath, _: bool) -> bool {
        true
    }

    async fn files(blobs: &BlobStore, root: TreeId) -> Vec<(String, ContentId)> {
        walk_tree(blobs, root, &all).await.unwrap().into_iter().map(|f| (f.path.to_string(), f.content_id.unwrap())).collect()
    }

    #[tokio::test]
    async fn basic_derivation() {
        let blobs = BlobStore::memory();
        let mut c1 = BTreeMap::new();
        c1.insert(p("a/b/c.txt"), change(b"c", FileKind::Regular));
        c1.insert(p("a/d.txt"), change(b"d", FileKind::Regular));
        c1.insert(p("top"), change(b"t", FileKind::Executable));
        let t1 = derive_tree(&blobs, None, &c1).await.unwrap();
        assert_eq!(files(&blobs, t1).await.len(), 3);
        let root = blobs.get_tree(&t1).await.unwrap().unwrap();
        assert_eq!(root.total_files(), 3);
        assert_eq!(root.total_size(), 3);

        // Delete the only file in a/b: the directory disappears.
        let mut c2 = BTreeMap::new();
        c2.insert(p("a/b/c.txt"), FileChange::Deletion);
        let t2 = derive_tree(&blobs, Some(t1), &c2).await.unwrap();
        assert!(lookup_path(&blobs, t2, &p("a/b")).await.unwrap().is_none());
        // Untouched subtree reused: `a` changed, but nothing else.
        let d = diff_trees(&blobs, Some(t1), Some(t2), &all).await.unwrap();
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].path, p("a/b/c.txt"));
        assert!(d[0].new.is_none());

        // file→dir: `top` becomes a directory.
        let mut c3 = BTreeMap::new();
        c3.insert(p("top/inner"), change(b"i", FileKind::Regular));
        let t3 = derive_tree(&blobs, Some(t2), &c3).await.unwrap();
        assert!(lookup_path(&blobs, t3, &p("top")).await.unwrap().unwrap().is_dir());

        // dir→file: `a` becomes a file.
        let mut c4 = BTreeMap::new();
        c4.insert(p("a"), change(b"now a file", FileKind::Regular));
        let t4 = derive_tree(&blobs, Some(t3), &c4).await.unwrap();
        assert_eq!(
            files(&blobs, t4).await,
            vec![("a".into(), ContentId::hash(b"now a file")), ("top/inner".into(), ContentId::hash(b"i"))]
        );

        // Delete everything.
        let mut c5 = BTreeMap::new();
        c5.insert(p("a"), FileChange::Deletion);
        c5.insert(p("top/inner"), FileChange::Deletion);
        let t5 = derive_tree(&blobs, Some(t4), &c5).await.unwrap();
        assert_eq!(t5, Tree::empty().id());
    }

    // Oracle test: random sequences of changes applied to a naive BTreeMap
    // file set must match the derived tree exactly (SPEC §14).
    fn arb_path() -> impl Strategy<Value = String> {
        prop::collection::vec(prop::sample::select(vec!["a", "b", "c", "a-b", "a.b"]), 1..4).prop_map(|v| v.join("/"))
    }

    fn arb_step() -> impl Strategy<Value = Vec<(String, Option<u8>)>> {
        prop::collection::vec((arb_path(), prop::option::weighted(0.7, any::<u8>())), 1..8)
    }

    fn apply_oracle(
        state: &mut BTreeMap<String, u8>,
        changes: &BTreeMap<RepoPath, FileChange>,
        contents: &BTreeMap<RepoPath, u8>,
    ) {
        for (path, ch) in changes {
            let ps = path.to_string();
            match ch {
                FileChange::Retain => {}
                FileChange::Deletion => {
                    state.remove(&ps);
                }
                FileChange::Change { .. } => {
                    // Implicit removals: ancestors that are files, and anything under this path.
                    let mut anc = path.parent();
                    while let Some(a) = anc {
                        state.remove(&a.to_string());
                        anc = a.parent();
                    }
                    let prefix = format!("{ps}/");
                    state.retain(|k, _| !k.starts_with(&prefix));
                    state.insert(ps, contents[path]);
                }
            }
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(200))]
        #[test]
        fn derived_tree_matches_oracle(steps in prop::collection::vec(arb_step(), 1..6)) {
            let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
            rt.block_on(async {
                let blobs = BlobStore::memory();
                let mut oracle: BTreeMap<String, u8> = BTreeMap::new();
                let mut tree: Option<TreeId> = None;
                for step in steps {
                    let mut changes = BTreeMap::new();
                    let mut contents = BTreeMap::new();
                    for (path, v) in step {
                        let rp = RepoPath::try_from(path.as_str()).unwrap();
                        match v {
                            Some(b) => { contents.insert(rp.clone(), b); changes.insert(rp, change(&[b], FileKind::Regular)); }
                            None => { changes.insert(rp, FileChange::Deletion); }
                        }
                    }
                    // Skip invalid changesets (file + file-underneath).
                    let cs = flux_core::Changeset {
                        parents: vec![], author: vec![], author_time: Default::default(), committer: None,
                        committer_time: None, message: vec![], file_changes: changes.clone(), extra: Default::default(),
                    };
                    if cs.validate().is_err() { continue; }
                    // Apply deletions first, then creations, in the oracle.
                    let (dels, adds): (BTreeMap<_, _>, BTreeMap<_, _>) =
                        changes.clone().into_iter().partition(|(_, c)| matches!(c, FileChange::Deletion));
                    apply_oracle(&mut oracle, &dels, &contents);
                    apply_oracle(&mut oracle, &adds, &contents);
                    let id = derive_tree(&blobs, tree, &changes).await.unwrap();
                    tree = Some(id);
                    let got: BTreeMap<String, ContentId> = files(&blobs, id).await.into_iter().collect();
                    let want: BTreeMap<String, ContentId> = oracle.iter().map(|(k, v)| (k.clone(), ContentId::hash(&[*v]))).collect();
                    prop_assert_eq!(got, want);
                    // Summaries are consistent.
                    let root = blobs.get_tree(&id).await.unwrap().unwrap();
                    prop_assert_eq!(root.total_files() as usize, oracle.len());
                }
                Ok(())
            })?;
        }
    }
}
