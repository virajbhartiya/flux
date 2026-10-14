//! Cached clone packs. Generating a pack for a whole history is the
//! expensive part of `git clone` (every object is produced, hashed,
//! deltified and compressed), so the server keeps one full pack per
//! repository and serves clones as that pack plus a freshly generated tail
//! for commits newer than it. The pack is rebuilt in the background once
//! the tail grows past `STALE_COMMITS`.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use flux_core::ChangesetId;
use flux_store::{GitPack, RepoId};

use super::http::{plan_pack, stream_pack};
use super::pack::PackWriter;
use crate::core::{Flux, FluxError, FluxResult};

/// Rebuild once this many commits are outside the cached pack.
const STALE_COMMITS: usize = 2_000;
/// And no more often than this.
const MIN_REBUILD_INTERVAL: Duration = Duration::from_secs(600);

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

impl Flux {
    /// The cached pack for a clone, if there is a usable one: `(pack, body)`
    /// where `body` is the pack without header and trailer.
    pub(crate) async fn git_cached_pack(&self, repo: RepoId) -> FluxResult<Option<(GitPack, Bytes)>> {
        let Some(pack) = self.meta.git_pack_get(repo).await? else { return Ok(None) };
        for t in &pack.tips {
            if self.graph_index(repo, t).await.is_err() {
                return Ok(None);
            }
        }
        if let Some((content, body)) = self.git.packs.get(&repo)
            && content == pack.content
        {
            return Ok(Some((pack, body)));
        }
        let Some(bytes) = self.blobs.get_content_bytes(&pack.content).await? else {
            tracing::warn!(repo = repo.0, "cached git pack content is missing");
            return Ok(None);
        };
        let Some((count, _)) = PackWriter::split(&bytes) else {
            return Err(FluxError::Internal(anyhow::anyhow!("cached git pack is malformed")));
        };
        if count != pack.objects {
            return Err(FluxError::Internal(anyhow::anyhow!("cached git pack object count mismatch")));
        }
        let body = bytes.slice(12..bytes.len() - 20);
        self.git.packs.insert(repo, (pack.content, body.clone()));
        Ok(Some((pack, body)))
    }

    /// Commits reachable from `heads` but not covered by the cached pack.
    async fn git_pack_tail(&self, repo: RepoId, heads: &[ChangesetId], pack: Option<&GitPack>) -> FluxResult<usize> {
        let g = self.repo_graph(repo).await?;
        let mut starts = Vec::new();
        for h in heads {
            starts.push(self.graph_index(repo, h).await?.1);
        }
        let mut hide = Vec::new();
        for t in pack.map(|p| p.tips.as_slice()).unwrap_or_default() {
            hide.push(self.graph_index(repo, t).await?.1);
        }
        let graph = g.graph.read().unwrap();
        Ok(graph.walk(&starts, &hide, false).count())
    }

    /// Rebuild the cached pack if it is missing or stale (and not rebuilt
    /// recently). Runs in the caller's task; meant for background use.
    pub async fn git_refresh_pack(self: &Arc<Self>, repo: RepoId) -> FluxResult<()> {
        let refs = self.meta.list_refs(repo, b"", 10_000).await?;
        let mut heads: Vec<ChangesetId> = refs.into_iter().map(|(_, v)| v.target).collect();
        heads.sort_unstable();
        heads.dedup();
        if heads.is_empty() {
            return Ok(());
        }
        let existing = self.meta.git_pack_get(repo).await?;
        if let Some(p) = &existing {
            if now_ms() - p.created_ms < MIN_REBUILD_INTERVAL.as_millis() as i64 {
                return Ok(());
            }
            if self.git_pack_tail(repo, &heads, Some(p)).await? < STALE_COMMITS {
                return Ok(());
            }
        }
        let started = std::time::Instant::now();
        let plan = plan_pack(self, repo, &heads, &[], false).await?;
        let objects = plan.len() as u32;
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let producer = {
            let flux = self.clone();
            tokio::spawn(async move { stream_pack(&flux, repo, plan, None, false, true, false, &tx).await })
        };
        let mut pack = Vec::new();
        while let Some(chunk) = rx.recv().await {
            pack.extend_from_slice(&chunk.map_err(|e| FluxError::Internal(e.into()))?);
        }
        producer.await.map_err(|e| FluxError::Internal(e.into()))??;
        let (content, _) = self.blobs.put_content(&pack).await?;
        let record = GitPack { content, objects, tips: heads, created_ms: now_ms() };
        self.meta.git_pack_put(repo, &record).await?;
        tracing::info!(
            repo = repo.0,
            objects,
            bytes = pack.len(),
            elapsed_ms = started.elapsed().as_millis() as u64,
            "git pack cached"
        );
        Ok(())
    }
}
