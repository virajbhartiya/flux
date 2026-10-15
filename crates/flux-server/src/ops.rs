//! Operational tooling: integrity checks and backups.
//!
//! `fsck` verifies that every changeset decodes and hashes to its id, that
//! the commit graph is closed (every parent exists), that every changeset's
//! root tree is derived and stored, and that every ref points at a known
//! changeset. With `full`, it also walks the trees at every ref head and
//! re-hashes every reachable tree and file. Missing derived data is repaired
//! by re-deriving it (derivation is deterministic).

use std::collections::HashSet;
use std::sync::Arc;

use flux_core::{ChangesetId, ContentId, ObjectKind, TreeId};
use flux_derive::TREE_V1;

use crate::core::{Flux, FluxResult};

#[derive(Debug, Default)]
pub struct FsckReport {
    pub repos: usize,
    pub changesets: u64,
    pub refs: usize,
    pub trees: u64,
    pub contents: u64,
    pub repaired: u64,
    pub problems: Vec<String>,
}

impl FsckReport {
    pub fn ok(&self) -> bool {
        self.problems.is_empty()
    }
}

impl Flux {
    pub async fn fsck(self: &Arc<Self>, full: bool, mut progress: impl FnMut(&str)) -> FluxResult<FsckReport> {
        let mut report = FsckReport::default();
        for info in self.meta.list_repos().await? {
            report.repos += 1;
            let repo = info.id;
            progress(&format!("{}: checking changesets", info.name));
            let expected = self.meta.changeset_count(repo).await?;
            let g = self.repo_graph(repo).await?;
            let ids: Vec<ChangesetId> = {
                let graph = g.graph.read().unwrap();
                (0..graph.len() as u32).map(|i| graph.id(i)).collect()
            };
            if ids.len() as u64 != expected {
                report.problems.push(format!(
                    "{}: commit graph has {} changesets but the store has {expected} (missing parents?)",
                    info.name,
                    ids.len()
                ));
            }
            for chunk in ids.chunks(1000) {
                match self.load_arc(repo, chunk).await {
                    Ok(loaded) => {
                        for (id, l) in chunk.iter().zip(&loaded) {
                            if l.is_none() {
                                report.problems.push(format!("{}: changeset {id} vanished", info.name));
                            }
                        }
                    }
                    Err(e) => report.problems.push(format!("{}: {e}", info.name)),
                }
                let derived = self.meta.get_derived(repo, TREE_V1, chunk).await?;
                for (id, d) in chunk.iter().zip(derived) {
                    let tree = match d {
                        Some(v) => match TreeId::from_slice(&v) {
                            Ok(t) => t,
                            Err(e) => {
                                report.problems.push(format!("{}: bad tree mapping for {id}: {e}", info.name));
                                continue;
                            }
                        },
                        None => {
                            // Repairable: derivation is deterministic.
                            match self.root_tree(repo, id).await {
                                Ok(t) => {
                                    report.repaired += 1;
                                    t
                                }
                                Err(e) => {
                                    report.problems.push(format!("{}: cannot derive tree for {id}: {e}", info.name));
                                    continue;
                                }
                            }
                        }
                    };
                    if !self.blobs.exists(ObjectKind::Tree, tree.as_bytes()).await? {
                        report.problems.push(format!("{}: root tree {tree} of {id} is missing", info.name));
                    }
                }
                report.changesets += chunk.len() as u64;
            }
            let refs = self.meta.list_refs(repo, b"", 100_000).await?;
            report.refs += refs.len();
            let known: HashSet<ChangesetId> = ids.iter().copied().collect();
            for (name, v) in &refs {
                if !known.contains(&v.target) {
                    report.problems.push(format!(
                        "{}: ref {} points at unknown changeset {}",
                        info.name,
                        String::from_utf8_lossy(name),
                        v.target
                    ));
                }
            }
            if full {
                progress(&format!("{}: verifying every tree and file reachable from {} refs", info.name, refs.len()));
                let mut seen_trees: HashSet<TreeId> = HashSet::new();
                let mut seen_contents: HashSet<ContentId> = HashSet::new();
                for (_, v) in &refs {
                    let Ok(root) = self.root_tree(repo, &v.target).await else { continue };
                    let mut stack = vec![root];
                    while let Some(t) = stack.pop() {
                        if !seen_trees.insert(t) {
                            continue;
                        }
                        let tree = match self.blobs.get_tree(&t).await {
                            Ok(Some(tree)) => tree,
                            Ok(None) => {
                                report.problems.push(format!("{}: tree {t} missing", info.name));
                                continue;
                            }
                            Err(e) => {
                                report.problems.push(format!("{}: tree {t}: {e}", info.name));
                                continue;
                            }
                        };
                        for e in tree.entries() {
                            if let Some(sub) = e.tree_id() {
                                stack.push(sub);
                            } else if let Some(c) = e.content_id()
                                && seen_contents.insert(c)
                            {
                                match self.blobs.get_content_bytes(&c).await {
                                    Ok(Some(data)) if data.len() as u64 == e.size => {}
                                    Ok(Some(data)) => report.problems.push(format!(
                                        "{}: content {c} has {} bytes, tree says {}",
                                        info.name,
                                        data.len(),
                                        e.size
                                    )),
                                    Ok(None) => report.problems.push(format!("{}: content {c} missing", info.name)),
                                    Err(e) => report.problems.push(format!("{}: content {c}: {e}", info.name)),
                                }
                            }
                        }
                    }
                }
                report.trees += seen_trees.len() as u64;
                report.contents += seen_contents.len() as u64;
            }
        }
        Ok(report)
    }

    /// Online, consistent backup into `dest` (all-in-one layout only): the
    /// metadata snapshot is taken first, then blobs are copied. Blobs are
    /// written before any metadata references them and are append-only, so
    /// everything the snapshot references is in the copy. Restore by
    /// starting `flux-server --data-dir <dest>`.
    pub async fn backup(&self, dest: &std::path::Path) -> FluxResult<(u64, u64)> {
        std::fs::create_dir_all(dest).map_err(|e| crate::core::FluxError::Internal(e.into()))?;
        self.meta.backup_to(&dest.join("meta.db")).await?;
        let st = self.blobs.snapshot_to(&dest.join("blobs")).map_err(crate::core::FluxError::Internal)?;
        Ok((st.objects as u64, st.bytes))
    }
}

/// Result of training one dictionary.
#[derive(Debug)]
pub struct DictReport {
    pub kind: ObjectKind,
    pub id: u16,
    pub samples: usize,
    /// Held-out samples compressed without and with the dictionary.
    pub plain_bytes: usize,
    pub dict_bytes: usize,
}

impl Flux {
    /// Train zstd dictionaries for trees and small files from the trees at
    /// every ref head of `repo`, and install them for new writes. Objects
    /// already stored keep their encoding.
    pub async fn train_dicts(&self, repo: flux_store::RepoId, max_samples: usize, size: usize) -> FluxResult<Vec<DictReport>> {
        use flux_store::dict::{DICT_MAX_OBJECT, train};
        // Trees from recent history (many versions of each directory), and
        // files from the ref heads.
        let refs = self.meta.list_refs(repo, b"", 10_000).await?;
        let heads: Vec<ChangesetId> = refs.into_iter().map(|(_, v)| v.target).collect();
        let recent: Vec<ChangesetId> = {
            let g = self.repo_graph(repo).await?;
            let mut starts = Vec::new();
            for h in &heads {
                starts.push(self.graph_index(repo, h).await?.1);
            }
            let graph = g.graph.read().unwrap();
            graph.walk(&starts, &[], false).take(2_000).map(|i| graph.id(i)).collect()
        };
        // Heads first, so their files are sampled before the size cap.
        let mut stack: Vec<TreeId> = self.root_trees(repo, &recent).await?;
        stack.reverse();
        stack.extend(self.root_trees(repo, &heads).await?);
        let (mut trees, mut files) = (Vec::new(), Vec::new());
        let (mut seen_t, mut seen_c) = (HashSet::new(), HashSet::new());
        while let Some(t) = stack.pop() {
            if !seen_t.insert(t) || trees.len() >= max_samples {
                continue;
            }
            let Some(bytes) = self.blobs.get_tree_bytes(&t).await? else { continue };
            if bytes.len() <= DICT_MAX_OBJECT {
                trees.push(bytes.to_vec());
            }
            let tree = self.blobs.get_tree(&t).await?.expect("just read");
            for e in tree.entries() {
                if let Some(sub) = e.tree_id() {
                    stack.push(sub);
                } else if let Some(c) = e.content_id()
                    && (e.size as usize) <= DICT_MAX_OBJECT
                    && files.len() < max_samples
                    && seen_c.insert(c)
                    && let Some(b) = self.blobs.get_content_bytes(&c).await?
                {
                    files.push(b.to_vec());
                }
            }
        }
        let mut reports = Vec::new();
        for (kind, mut samples) in [(ObjectKind::Tree, trees), (ObjectKind::Content, files)] {
            if samples.len() < 64 {
                continue;
            }
            // Hold out every 10th sample to measure the gain honestly.
            let held: Vec<Vec<u8>> = samples.iter().step_by(10).cloned().collect();
            let mut i = 0;
            samples.retain(|_| {
                i += 1;
                (i - 1) % 10 != 0
            });
            let raw = train(&samples, size)?;
            let plain_bytes = held.iter().map(|s| zstd::bulk::compress(s, 3).map_or(s.len(), |c| c.len()).min(s.len())).sum();
            let mut c =
                zstd::bulk::Compressor::with_dictionary(3, &raw).map_err(|e| crate::core::FluxError::Internal(e.into()))?;
            let dict_bytes = held.iter().map(|s| c.compress(s).map_or(s.len(), |c| c.len()).min(s.len())).sum();
            let id = self.blobs.install_dict(kind, raw).await?;
            reports.push(DictReport { kind, id, samples: samples.len(), plain_bytes, dict_bytes });
        }
        Ok(reports)
    }
}
