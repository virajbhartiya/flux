//! History queries on top of the commit graph and `lastmod_v1`: log pages,
//! file/directory history, ancestry, merge bases and blame.

use std::collections::{BinaryHeap, HashSet};
use std::sync::{Arc, RwLock};

use flux_core::{ChangesetId, ContentId, DerivedId, RepoPath, TreeId};
use flux_derive::{LASTMOD_V1, LmEntry, LmWriter, Side, derive_lastmod, lookup_lastmod};
use flux_store::{GraphRow, NewChangeset, RepoId};

use crate::core::{Event, Flux, FluxError, FluxResult, Loaded};
use crate::graph::Graph;

pub struct RepoGraph {
    pub graph: RwLock<Graph>,
    /// Serializes loads/refreshes from the metadata store.
    load: tokio::sync::Mutex<()>,
}

/// Line attribution for one file version.
#[derive(Debug, Clone)]
pub struct Blame {
    /// `(first line, line count, changeset)`, 0-based, covering every line.
    pub ranges: Vec<(u32, u32, ChangesetId)>,
    pub lines: u32,
}

/// How many lastmod derivations to buffer before flushing.
const LASTMOD_BATCH: usize = 512;

impl Flux {
    // ───────────── commit graph ─────────────

    pub async fn repo_graph(&self, repo: RepoId) -> FluxResult<Arc<RepoGraph>> {
        let g = {
            let mut map = self.graphs.lock().unwrap();
            map.entry(repo)
                .or_insert_with(|| {
                    Arc::new(RepoGraph { graph: RwLock::new(Graph::default()), load: tokio::sync::Mutex::new(()) })
                })
                .clone()
        };
        if g.graph.read().unwrap().loaded_seq == 0 {
            self.refresh_graph(repo, &g).await?;
        }
        Ok(g)
    }

    async fn refresh_graph(&self, repo: RepoId, g: &RepoGraph) -> FluxResult<()> {
        let _guard = g.load.lock().await;
        let after = g.graph.read().unwrap().loaded_seq;
        let mut rows = Vec::new();
        let mut cursor = after;
        loop {
            let batch = self.meta.graph_tail(repo, cursor, 50_000).await?;
            let Some(last) = batch.last() else { break };
            cursor = last.seq;
            rows.extend(batch);
        }
        if !rows.is_empty() {
            g.graph.write().unwrap().extend(&rows);
        }
        Ok(())
    }

    /// Graph index of `id`, tailing the log once if it's not known yet (it
    /// may have been created by another server instance).
    pub async fn graph_index(&self, repo: RepoId, id: &ChangesetId) -> FluxResult<(Arc<RepoGraph>, u32)> {
        let g = self.repo_graph(repo).await?;
        let found = g.graph.read().unwrap().get(id);
        if let Some(i) = found {
            return Ok((g, i));
        }
        self.refresh_graph(repo, &g).await?;
        let i = g.graph.read().unwrap().get(id);
        match i {
            Some(i) => Ok((g, i)),
            None => Err(FluxError::NotFound(format!("changeset {id} not found"))),
        }
    }

    pub(crate) async fn after_create(self: &Arc<Self>, repo: RepoId, rows: &[NewChangeset], seqs: &[i64]) {
        let has_graph = self.graphs.lock().unwrap().get(&repo).cloned();
        if let Some(g) = has_graph {
            let mut graph = g.graph.write().unwrap();
            // Only append contiguously; otherwise the next query refreshes.
            if seqs.first().is_some_and(|s| *s == graph.loaded_seq + 1) {
                let grows: Vec<GraphRow> = rows
                    .iter()
                    .zip(seqs)
                    .map(|(r, s)| GraphRow { seq: *s, id: r.id, gen_num: r.gen_num, parents: r.parents.clone() })
                    .collect();
                graph.extend(&grows);
            }
        }
        if let Some(last) = rows.last() {
            let name = self.repo_name(repo).await;
            self.publish(Event::Changesets { repo: name, count: rows.len(), last: last.id });
            // Derive lastmod in the background so history and the web UI
            // never wait for it.
            let (this, tip) = (self.clone(), last.id);
            tokio::spawn(async move {
                if let Err(e) = this.lastmod(repo, &tip).await {
                    tracing::warn!(error = %e, "background lastmod derivation failed");
                }
            });
        }
    }

    pub async fn is_ancestor(&self, repo: RepoId, a: &ChangesetId, b: &ChangesetId) -> FluxResult<bool> {
        let (g, ib) = self.graph_index(repo, b).await?;
        let (_, ia) = self.graph_index(repo, a).await?;
        Ok(g.graph.read().unwrap().is_ancestor(ia, ib))
    }

    pub async fn merge_base(&self, repo: RepoId, a: &ChangesetId, b: &ChangesetId) -> FluxResult<Option<ChangesetId>> {
        let (g, ia) = self.graph_index(repo, a).await?;
        let (_, ib) = self.graph_index(repo, b).await?;
        let graph = g.graph.read().unwrap();
        Ok(graph.merge_base(ia, ib).map(|i| graph.id(i)))
    }

    pub async fn ahead_behind(&self, repo: RepoId, a: &ChangesetId, b: &ChangesetId) -> FluxResult<(usize, usize)> {
        // Changesets are immutable, so the answer for a pair never changes.
        if let Some(r) = self.ahead_behind_cache.get(&(repo, *a, *b)) {
            return Ok(r);
        }
        let (g, ia) = self.graph_index(repo, a).await?;
        let (_, ib) = self.graph_index(repo, b).await?;
        let r = g.graph.read().unwrap().ahead_behind(ia, ib);
        self.ahead_behind_cache.insert((repo, *a, *b), r);
        Ok(r)
    }

    /// One page of history: ancestors of `start` not reachable from `hide`,
    /// newest generation first. With `path`, only changesets that modified
    /// that file or directory (via `lastmod_v1`).
    pub async fn log_page(
        &self,
        repo: RepoId,
        start: &ChangesetId,
        hide: &[ChangesetId],
        path: Option<&RepoPath>,
        first_parent: bool,
        skip: usize,
        limit: usize,
    ) -> FluxResult<Vec<Arc<Loaded>>> {
        let ids = match path {
            Some(p) => self.path_history(repo, start, p, first_parent, skip + limit).await?.into_iter().skip(skip).collect(),
            None => {
                let (g, is) = self.graph_index(repo, start).await?;
                let mut ih = Vec::with_capacity(hide.len());
                for h in hide {
                    ih.push(self.graph_index(repo, h).await?.1);
                }
                let graph = g.graph.read().unwrap();
                graph.walk(&[is], &ih, first_parent).skip(skip).take(limit).map(|i| graph.id(i)).collect::<Vec<_>>()
            }
        };
        let ids: Vec<ChangesetId> = ids.into_iter().take(limit).collect();
        Ok(self.load_arc(repo, &ids).await?.into_iter().flatten().collect())
    }

    // ───────────── lastmod ─────────────

    async fn lastmod_done(&self, repo: RepoId) -> FluxResult<Arc<std::sync::Mutex<HashSet<ChangesetId>>>> {
        if let Some(d) = self.lastmod_done.lock().unwrap().get(&repo) {
            return Ok(d.clone());
        }
        let ids = self.meta.derived_ids(repo, LASTMOD_V1).await?;
        let set = Arc::new(std::sync::Mutex::new(ids.into_iter().collect::<HashSet<_>>()));
        Ok(self.lastmod_done.lock().unwrap().entry(repo).or_insert(set).clone())
    }

    fn lastmod_lock(&self, repo: RepoId) -> Arc<tokio::sync::Mutex<()>> {
        self.lastmod_locks.lock().unwrap().entry(repo).or_default().clone()
    }

    /// Root lastmod node of `cs`, deriving it (and any underived ancestors,
    /// in topological order) if needed.
    pub async fn lastmod(&self, repo: RepoId, cs: &ChangesetId) -> FluxResult<DerivedId> {
        if let Some(v) = self.derived_get(repo, LASTMOD_V1, cs).await? {
            return Ok(DerivedId::from_bytes(v));
        }
        let lock = self.lastmod_lock(repo);
        let _guard = lock.lock().await;
        // Which changesets already have lastmod: loaded once per repo (one
        // scan), then maintained in memory as we derive.
        let done = self.lastmod_done(repo).await?;
        let (g, start) = self.graph_index(repo, cs).await?;
        let mut todo: Vec<u32> = Vec::new();
        {
            let graph = g.graph.read().unwrap();
            let done = done.lock().unwrap();
            let mut stack = vec![start];
            let mut seen: HashSet<u32> = HashSet::from([start]);
            while let Some(i) = stack.pop() {
                if done.contains(&graph.id(i)) {
                    continue;
                }
                todo.push(i);
                for &p in graph.parents(i) {
                    if seen.insert(p) {
                        stack.push(p);
                    }
                }
            }
        }
        if todo.is_empty() {
            // Raced with another deriver.
            let v = self.meta.get_derived(repo, LASTMOD_V1, std::slice::from_ref(cs)).await?.pop().flatten();
            return DerivedId::from_slice(&v.ok_or_else(|| FluxError::Internal(anyhow::anyhow!("lastmod vanished")))?)
                .map_err(|e| FluxError::Internal(e.into()));
        }
        // Graph indices are in log order, which is topological.
        todo.sort_unstable();
        let mut known: std::collections::HashMap<ChangesetId, DerivedId> = std::collections::HashMap::new();
        let started = std::time::Instant::now();
        let total = todo.len();
        for chunk in todo.chunks(LASTMOD_BATCH) {
            let writer = LmWriter::new(&self.blobs);
            let mut rows: Vec<(ChangesetId, Vec<u8>)> = Vec::with_capacity(chunk.len());
            let items: Vec<(ChangesetId, Vec<ChangesetId>)> = {
                let graph = g.graph.read().unwrap();
                chunk.iter().map(|&i| (graph.id(i), graph.parents(i).iter().map(|p| graph.id(*p)).collect())).collect()
            };
            // Batch-fetch every root tree and external parent lastmod the
            // chunk needs (one query each instead of one per changeset).
            let mut want_trees: Vec<ChangesetId> =
                items.iter().flat_map(|(id, ps)| std::iter::once(*id).chain(ps.iter().copied())).collect();
            want_trees.sort_unstable();
            want_trees.dedup();
            let mut trees: std::collections::HashMap<ChangesetId, TreeId> = std::collections::HashMap::new();
            for part in want_trees.chunks(500) {
                for (id, v) in part.iter().zip(self.meta.get_derived(repo, flux_derive::TREE_V1, part).await?) {
                    let t = match v {
                        Some(v) => TreeId::from_slice(&v).map_err(|e| FluxError::Internal(e.into()))?,
                        None => self.root_tree(repo, id).await?,
                    };
                    trees.insert(*id, t);
                }
            }
            let mut want_lm: Vec<ChangesetId> =
                items.iter().flat_map(|(_, ps)| ps.iter().copied()).filter(|p| !known.contains_key(p)).collect();
            want_lm.sort_unstable();
            want_lm.dedup();
            let in_chunk: HashSet<ChangesetId> = items.iter().map(|(id, _)| *id).collect();
            want_lm.retain(|p| !in_chunk.contains(p));
            for part in want_lm.chunks(500) {
                for (id, v) in part.iter().zip(self.meta.get_derived(repo, LASTMOD_V1, part).await?) {
                    let v = v.ok_or_else(|| FluxError::Internal(anyhow::anyhow!("parent lastmod missing for {id}")))?;
                    known.insert(*id, DerivedId::from_slice(&v).map_err(|e| FluxError::Internal(e.into()))?);
                }
            }
            for (id, parents) in &items {
                let sides: Vec<Side> = parents.iter().map(|p| Side { tree: trees[p], lm: known[p] }).collect();
                let root =
                    derive_lastmod(&self.blobs, &writer, *id, trees[id], sides.first().copied(), sides.get(1..).unwrap_or(&[]))
                        .await?;
                known.insert(*id, root);
                rows.push((*id, root.as_bytes().to_vec()));
            }
            writer.flush().await?;
            self.blobs.sync().await?;
            self.meta.put_derived(repo, LASTMOD_V1, &rows).await?;
            done.lock().unwrap().extend(rows.iter().map(|r| r.0));
        }
        if total > 1000 {
            tracing::info!(
                repo = repo.0,
                changesets = total,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "derived lastmod"
            );
        }
        known.get(cs).copied().ok_or_else(|| FluxError::Internal(anyhow::anyhow!("lastmod not derived")))
    }

    /// The changeset that last modified `path` as of `cs`.
    pub async fn last_modified(&self, repo: RepoId, cs: &ChangesetId, path: &RepoPath) -> FluxResult<Option<LmEntry>> {
        let root = self.lastmod(repo, cs).await?;
        Ok(lookup_lastmod(&self.blobs, root, path).await?)
    }

    /// Changesets that modified `path`, newest first (up to `limit`).
    pub async fn path_history(
        &self,
        repo: RepoId,
        start: &ChangesetId,
        path: &RepoPath,
        first_parent: bool,
        limit: usize,
    ) -> FluxResult<Vec<ChangesetId>> {
        let mut out = Vec::new();
        let Some(first) = self.last_modified(repo, start, path).await? else { return Ok(out) };
        let (g, _) = self.graph_index(repo, start).await?;
        let mut heap: BinaryHeap<(u64, ChangesetId)> = BinaryHeap::new();
        let mut seen: HashSet<ChangesetId> = HashSet::new();
        let gen_of = |id: &ChangesetId| {
            let graph = g.graph.read().unwrap();
            graph.get(id).map(|i| graph.gen_of(i)).unwrap_or(0)
        };
        seen.insert(first.cs);
        heap.push((gen_of(&first.cs), first.cs));
        while let Some((_, c)) = heap.pop() {
            out.push(c);
            if out.len() >= limit {
                break;
            }
            let parents: Vec<ChangesetId> = {
                let graph = g.graph.read().unwrap();
                match graph.get(&c) {
                    Some(i) => graph.parents(i).iter().map(|p| graph.id(*p)).collect(),
                    None => vec![],
                }
            };
            let parents = if first_parent { &parents[..parents.len().min(1)] } else { &parents[..] };
            for p in parents {
                if let Some(e) = self.last_modified(repo, p, path).await?
                    && seen.insert(e.cs)
                {
                    heap.push((gen_of(&e.cs), e.cs));
                }
            }
        }
        Ok(out)
    }

    // ───────────── blame ─────────────

    /// Attribute every line of `path` at `cs` to the changeset that last
    /// changed it, following the file's first-parent history.
    pub async fn blame(&self, repo: RepoId, cs: &ChangesetId, path: &RepoPath) -> FluxResult<Arc<Blame>> {
        let Some(top) = self.last_modified(repo, cs, path).await? else {
            return Err(FluxError::NotFound(format!("{path} not found")));
        };
        let key = (repo, top.cs, path.as_bytes().to_vec());
        if let Some(b) = self.blame_cache.get(&key) {
            return Ok(b);
        }
        const MAX_VERSIONS: usize = 2_000;
        let t0 = std::time::Instant::now();
        // Versions newest → oldest: the changesets that modified the path
        // (first-parent, from the history index), with the content each one
        // wrote, which the changeset itself names (no tree walk needed).
        let ids = self.path_history(repo, &top.cs, path, true, MAX_VERSIONS).await?;
        let loaded = self.load_arc(repo, &ids).await?;
        let mut versions: Vec<(ChangesetId, ContentId)> = Vec::with_capacity(ids.len());
        for (c, l) in ids.iter().zip(loaded) {
            let direct = l.as_ref().and_then(|l| match l.changeset.file_changes.get(path) {
                Some(flux_core::FileChange::Change { content_id, kind, .. })
                    if !matches!(kind, flux_core::FileKind::GitLink(_)) =>
                {
                    Some(*content_id)
                }
                _ => None,
            });
            let cid = match direct {
                Some(c) => c,
                // Merges that kept a side (`Retain`): read it from the tree.
                None => match self.entry_at(repo, c, path).await?.and_then(|e| e.content_id()) {
                    Some(c) => c,
                    None => break,
                },
            };
            versions.push((*c, cid));
        }
        let t_walk = t0.elapsed();
        let texts: Vec<bytes::Bytes> = {
            use futures::StreamExt as _;
            let futs: Vec<_> = versions
                .iter()
                .map(|(_, cid)| {
                    let (blobs, cid) = (self.blobs.clone(), *cid);
                    async move {
                        blobs
                            .get_content_bytes(&cid)
                            .await?
                            .ok_or_else(|| FluxError::Internal(anyhow::anyhow!("content {cid} missing")))
                    }
                })
                .collect();
            futures::stream::iter(futs).buffered(32).collect::<Vec<_>>().await.into_iter().collect::<FluxResult<_>>()?
        };
        let t_fetch = t0.elapsed();
        let ids: Vec<ChangesetId> = versions.iter().map(|v| v.0).collect();
        let blame =
            tokio::task::spawn_blocking(move || compute_blame(&ids, &texts)).await.map_err(|e| FluxError::Internal(e.into()))?;
        tracing::debug!(
            versions = versions.len(),
            walk_ms = t_walk.as_millis() as u64,
            fetch_ms = (t_fetch - t_walk).as_millis() as u64,
            compute_ms = (t0.elapsed() - t_fetch).as_millis() as u64,
            "blame computed"
        );
        let blame = Arc::new(blame);
        self.blame_cache.insert(key, blame.clone());
        Ok(blame)
    }

    /// Root tree of a changeset (the web layer's entry point).
    pub async fn tree_of(&self, repo: RepoId, cs: &ChangesetId) -> FluxResult<TreeId> {
        self.root_tree(repo, cs).await
    }
}

/// `versions[0]` is the newest. Each line of the newest text is attributed
/// to the oldest version in which it still appears unchanged.
fn compute_blame(versions: &[ChangesetId], texts: &[bytes::Bytes]) -> Blame {
    fn split(b: &[u8]) -> Vec<&[u8]> {
        let mut v: Vec<&[u8]> = b.split_inclusive(|c| *c == b'\n').collect();
        if v.is_empty() {
            v.push(&[]);
        }
        v
    }
    let newest = split(&texts[0]);
    let n = newest.len();
    let mut owner: Vec<Option<usize>> = vec![None; n];
    // pos[k] = position in the current version of newest line k (None once attributed).
    let mut pos: Vec<Option<usize>> = (0..n).map(Some).collect();
    for k in 0..texts.len() {
        if pos.iter().all(Option::is_none) {
            break;
        }
        if k + 1 == texts.len() {
            for (line, p) in pos.iter_mut().enumerate() {
                if p.take().is_some() {
                    owner[line] = Some(k);
                }
            }
            break;
        }
        let cur = split(&texts[k]);
        let older = split(&texts[k + 1]);
        // Map current-version line → older-version line for unchanged lines.
        let mut map: Vec<Option<usize>> = vec![None; cur.len()];
        let diff = similar::capture_diff_slices(similar::Algorithm::Myers, &older, &cur);
        for op in diff {
            if let similar::DiffOp::Equal { old_index, new_index, len } = op {
                for d in 0..len {
                    map[new_index + d] = Some(old_index + d);
                }
            }
        }
        for (line, p) in pos.iter_mut().enumerate() {
            if let Some(cp) = *p {
                match map.get(cp).copied().flatten() {
                    Some(op) => *p = Some(op),
                    None => {
                        owner[line] = Some(k);
                        *p = None;
                    }
                }
            }
        }
    }
    let mut ranges: Vec<(u32, u32, ChangesetId)> = Vec::new();
    for (line, o) in owner.iter().enumerate() {
        let cs = versions[o.unwrap_or(texts.len() - 1)];
        match ranges.last_mut() {
            Some(r) if r.2 == cs && (r.0 + r.1) as usize == line => r.1 += 1,
            _ => ranges.push((line as u32, 1, cs)),
        }
    }
    Blame { ranges, lines: n as u32 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blame_attributes_lines() {
        let c = |n: u8| ChangesetId::hash(&[n]);
        let v1 = bytes::Bytes::from_static(b"a\nb\nc\n");
        let v2 = bytes::Bytes::from_static(b"a\nB\nc\n");
        let v3 = bytes::Bytes::from_static(b"a\nB\nc\nd\n");
        let b = compute_blame(&[c(3), c(2), c(1)], &[v3, v2, v1]);
        assert_eq!(b.lines, 4);
        assert_eq!(b.ranges, vec![(0, 1, c(1)), (1, 1, c(2)), (2, 1, c(1)), (3, 1, c(3))]);
    }
}
