//! `lastmod_v1`: for every file and directory, the changeset that last
//! modified it (SPEC §6.2, the role of `pathhist_v1`).
//!
//! A lastmod node mirrors one tree: entry `i` names the same child as tree
//! entry `i` and records `cs`, the changeset that produced that child's
//! current value, plus the child's lastmod node for directories.
//!
//! Derivation for changeset `C` with parents `p0..pn` is incremental, like
//! `tree_v1`: subtrees equal to `p0`'s reuse `p0`'s lastmod node verbatim.
//! For merges, a child equal to some other parent's child inherits that
//! parent's attribution (the change came from that branch); anything else
//! changed in `C` is attributed to `C`. Cost is O(changes × depth).
//!
//! Encoding: `[1, [[name: bytes, cs: bytes32, sub: bytes32 | null], ...]]`,
//! content-addressed as a `Derived` object.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use bytes::Bytes;
use flux_core::cbor::{Decoder, Encoder};
use flux_core::{ChangesetId, DerivedId, ObjectKind, PathComponent, RepoPath, Tree, TreeEntry, TreeId};
use flux_store::{BlobStore, PutObject};

use crate::{DeriveError, DeriveResult};

type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LmEntry {
    pub name: PathComponent,
    pub cs: ChangesetId,
    pub sub: Option<DerivedId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LmNode {
    pub entries: Vec<LmEntry>,
}

impl LmNode {
    pub fn encode(&self) -> Vec<u8> {
        let mut e = Encoder::new();
        e.array(2).uint(1).array(self.entries.len());
        for en in &self.entries {
            e.array(3).bytes(en.name.as_bytes()).bytes(en.cs.as_bytes());
            match &en.sub {
                Some(s) => e.bytes(s.as_bytes()),
                None => e.null(),
            };
        }
        e.into_bytes()
    }

    pub fn decode(data: &[u8]) -> DeriveResult<Self> {
        let mut d = Decoder::new(data);
        d.array_exact(2)?;
        if d.uint()? != 1 {
            return Err(flux_core::Error::Decode("unsupported lastmod version".into()).into());
        }
        let n = d.array()?;
        let mut entries = Vec::with_capacity(n);
        for _ in 0..n {
            d.array_exact(3)?;
            let name = PathComponent::new(d.bytes()?)?;
            let cs = ChangesetId::from_bytes(d.bytes_fixed()?);
            let sub = if d.null()? { None } else { Some(DerivedId::from_bytes(d.bytes_fixed()?)) };
            d.end();
            entries.push(LmEntry { name, cs, sub });
        }
        d.end();
        d.end();
        d.finish()?;
        Ok(Self { entries })
    }

    pub fn get(&self, name: &[u8]) -> Option<&LmEntry> {
        self.entries.binary_search_by(|e| e.name.as_bytes().cmp(name)).ok().map(|i| &self.entries[i])
    }
}

/// Reads lastmod nodes, seeing nodes buffered in this writer first.
pub struct LmWriter<'a> {
    blobs: &'a BlobStore,
    pending: std::sync::Mutex<HashMap<DerivedId, (Arc<LmNode>, Bytes, Option<DerivedId>)>>,
}

impl<'a> LmWriter<'a> {
    pub fn new(blobs: &'a BlobStore) -> Self {
        Self { blobs, pending: Default::default() }
    }

    pub async fn get(&self, id: DerivedId) -> DeriveResult<Arc<LmNode>> {
        if let Some((n, _, _)) = self.pending.lock().unwrap().get(&id) {
            return Ok(n.clone());
        }
        load_lm(self.blobs, id).await
    }

    fn put(&self, node: LmNode, base: Option<DerivedId>) -> DerivedId {
        let bytes = Bytes::from(node.encode());
        let id = DerivedId::hash(&bytes);
        self.pending.lock().unwrap().entry(id).or_insert_with(|| (Arc::new(node), bytes, base.filter(|b| *b != id)));
        id
    }

    /// Write buffered nodes. Callers must `BlobStore::sync` before recording
    /// derived rows that reference them.
    pub async fn flush(&self) -> DeriveResult<usize> {
        let items: Vec<PutObject> = self
            .pending
            .lock()
            .unwrap()
            .drain()
            .map(|(id, (_, bytes, base))| PutObject {
                kind: ObjectKind::Derived,
                id: *id.as_bytes(),
                data: bytes,
                base: base.map(|b| *b.as_bytes()),
            })
            .collect();
        let n = items.len();
        self.blobs.put_objects(items).await?;
        Ok(n)
    }
}

pub async fn load_lm(blobs: &BlobStore, id: DerivedId) -> DeriveResult<Arc<LmNode>> {
    let bytes = blobs.get_raw(ObjectKind::Derived, id.as_bytes()).await?.ok_or(DeriveError::MissingDerived(id))?;
    Ok(Arc::new(LmNode::decode(&bytes)?))
}

/// One side of the derivation: a directory's tree and its lastmod node.
#[derive(Clone, Copy)]
pub struct Side {
    pub tree: TreeId,
    pub lm: DerivedId,
}

async fn load_tree<F>(trees: &F, id: TreeId) -> DeriveResult<Arc<Tree>>
where
    F: Fn(TreeId) -> BoxFut<'static, DeriveResult<Arc<Tree>>> + Sync,
{
    trees(id).await
}

/// Derive the lastmod root for changeset `cs` whose root tree is `tree`.
/// `p0` is the first parent's side, `others` the remaining parents'.
pub async fn derive_lastmod(
    blobs: &BlobStore,
    writer: &LmWriter<'_>,
    cs: ChangesetId,
    tree: TreeId,
    p0: Option<Side>,
    others: &[Side],
) -> DeriveResult<DerivedId> {
    let trees = |id: TreeId| -> BoxFut<'static, DeriveResult<Arc<Tree>>> {
        let blobs = blobs.clone();
        Box::pin(async move { blobs.get_tree(&id).await?.ok_or(DeriveError::MissingTree(id)) })
    };
    if let Some(s) = p0
        && s.tree == tree
    {
        return Ok(s.lm);
    }
    dir(writer, &trees, cs, tree, p0, others.to_vec()).await
}

fn dir<'a, F>(
    writer: &'a LmWriter<'a>,
    trees: &'a F,
    cs: ChangesetId,
    tree: TreeId,
    base: Option<Side>,
    others: Vec<Side>,
) -> BoxFut<'a, DeriveResult<DerivedId>>
where
    F: Fn(TreeId) -> BoxFut<'static, DeriveResult<Arc<Tree>>> + Sync,
{
    Box::pin(async move {
        let t = load_tree(trees, tree).await?;
        let (bt, bl) = match base {
            Some(s) => (Some(load_tree(trees, s.tree).await?), Some(writer.get(s.lm).await?)),
            None => (None, None),
        };
        let mut other_nodes: Vec<(Arc<Tree>, Arc<LmNode>)> = Vec::with_capacity(others.len());
        for o in &others {
            other_nodes.push((load_tree(trees, o.tree).await?, writer.get(o.lm).await?));
        }
        let find = |tr: &Tree, lm: &LmNode, name: &[u8]| -> Option<(TreeEntry, LmEntry)> {
            let te = tr.get(name)?.clone();
            let le = lm.get(name)?.clone();
            Some((te, le))
        };
        let mut entries = Vec::with_capacity(t.entries().len());
        let mut recurse: Vec<(usize, TreeId, Option<Side>, Vec<Side>)> = Vec::new();
        for e in t.entries() {
            let name = e.name.as_bytes();
            let from_base = match (&bt, &bl) {
                (Some(bt), Some(bl)) => find(bt, bl, name),
                _ => None,
            };
            if let Some((be, le)) = &from_base
                && be == e
            {
                entries.push(le.clone());
                continue;
            }
            // A merge child equal to another parent's child came from there.
            let inherited =
                other_nodes.iter().find_map(|(ot, ol)| find(ot, ol, name).filter(|(oe, _)| oe == e).map(|(_, le)| le));
            if let Some(le) = inherited {
                entries.push(le);
                continue;
            }
            match e.tree_id() {
                Some(sub_tree) => {
                    let side = |pair: Option<(TreeEntry, LmEntry)>| match pair {
                        Some((te, le)) => match (te.tree_id(), le.sub) {
                            (Some(t), Some(l)) => Some(Side { tree: t, lm: l }),
                            _ => None,
                        },
                        None => None,
                    };
                    let sub_base = side(from_base);
                    let sub_others: Vec<Side> = other_nodes.iter().filter_map(|(ot, ol)| side(find(ot, ol, name))).collect();
                    recurse.push((entries.len(), sub_tree, sub_base, sub_others));
                    entries.push(LmEntry { name: e.name.clone(), cs, sub: None });
                }
                None => entries.push(LmEntry { name: e.name.clone(), cs, sub: None }),
            }
        }
        let subs =
            futures::future::try_join_all(recurse.iter().map(|(_, t, b, o)| dir(writer, trees, cs, *t, *b, o.clone()))).await?;
        for ((i, ..), sub) in recurse.iter().zip(subs) {
            entries[*i].sub = Some(sub);
        }
        Ok(writer.put(LmNode { entries }, base.map(|b| b.lm)))
    })
}

/// The lastmod entry for `path` under root node `root`.
pub async fn lookup_lastmod(blobs: &BlobStore, root: DerivedId, path: &RepoPath) -> DeriveResult<Option<LmEntry>> {
    let comps: Vec<&[u8]> = path.components().collect();
    let mut node = load_lm(blobs, root).await?;
    for (i, c) in comps.iter().enumerate() {
        let Some(e) = node.get(c).cloned() else { return Ok(None) };
        if i == comps.len() - 1 {
            return Ok(Some(e));
        }
        match e.sub {
            Some(s) => node = load_lm(blobs, s).await?,
            None => return Ok(None),
        }
    }
    Ok(None)
}

/// The lastmod node for directory `path` (`None` = root).
pub async fn lastmod_dir(blobs: &BlobStore, root: DerivedId, path: Option<&RepoPath>) -> DeriveResult<Option<Arc<LmNode>>> {
    match path {
        None => Ok(Some(load_lm(blobs, root).await?)),
        Some(p) => match lookup_lastmod(blobs, root, p).await? {
            Some(LmEntry { sub: Some(s), .. }) => Ok(Some(load_lm(blobs, s).await?)),
            _ => Ok(None),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::derive_tree;
    use flux_core::{ContentId, FileChange, FileKind};
    use std::collections::BTreeMap;

    fn p(s: &str) -> RepoPath {
        RepoPath::try_from(s).unwrap()
    }

    fn set(path: &str, content: &[u8]) -> (RepoPath, FileChange) {
        (
            p(path),
            FileChange::Change {
                content_id: ContentId::hash(content),
                size: content.len() as u64,
                kind: FileKind::Regular,
                copy_from: None,
            },
        )
    }

    #[tokio::test]
    async fn attribution_linear_and_merge() {
        let blobs = BlobStore::memory();
        let w = LmWriter::new(&blobs);
        let c = |n: u8| ChangesetId::hash(&[n]);
        // c1: a, d/x, d/y
        let t1 = derive_tree(&blobs, None, &BTreeMap::from([set("a", b"1"), set("d/x", b"1"), set("d/y", b"1")])).await.unwrap();
        let l1 = derive_lastmod(&blobs, &w, c(1), t1, None, &[]).await.unwrap();
        // c2 (on c1): d/x changes
        let t2 = derive_tree(&blobs, Some(t1), &BTreeMap::from([set("d/x", b"2")])).await.unwrap();
        let l2 = derive_lastmod(&blobs, &w, c(2), t2, Some(Side { tree: t1, lm: l1 }), &[]).await.unwrap();
        // c3 (on c1, side branch): a changes
        let t3 = derive_tree(&blobs, Some(t1), &BTreeMap::from([set("a", b"3")])).await.unwrap();
        let l3 = derive_lastmod(&blobs, &w, c(3), t3, Some(Side { tree: t1, lm: l1 }), &[]).await.unwrap();
        // c4 = merge(c2, c3) taking c3's `a`.
        let t4 = derive_tree(&blobs, Some(t2), &BTreeMap::from([set("a", b"3")])).await.unwrap();
        let l4 =
            derive_lastmod(&blobs, &w, c(4), t4, Some(Side { tree: t2, lm: l2 }), &[Side { tree: t3, lm: l3 }]).await.unwrap();
        w.flush().await.unwrap();
        let at = |root, path: &str| {
            let blobs = blobs.clone();
            let path = p(path);
            async move { lookup_lastmod(&blobs, root, &path).await.unwrap().unwrap().cs }
        };
        assert_eq!(at(l2, "d/x").await, c(2));
        assert_eq!(at(l2, "d/y").await, c(1));
        assert_eq!(at(l2, "d").await, c(2));
        assert_eq!(at(l2, "a").await, c(1));
        assert_eq!(at(l4, "a").await, c(3), "merged-in change keeps its branch's attribution");
        assert_eq!(at(l4, "d/x").await, c(2));
        // Node round-trips.
        let n = load_lm(&blobs, l4).await.unwrap();
        assert_eq!(LmNode::decode(&n.encode()).unwrap(), *n);
    }
}
