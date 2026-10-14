//! Rust client for flux. Every object fetched is verified against its id, so
//! callers can trust content regardless of which tier served it.

use std::collections::HashMap;

use anyhow::{Context, Result, anyhow, bail};
use bytes::Bytes;
use flux_core::{Changeset, ChangesetId, ContentId, Tree, TreeId};
use flux_proto::v1::admin_service_client::AdminServiceClient;
use flux_proto::v1::object_service_client::ObjectServiceClient;
use flux_proto::v1::repo_service_client::RepoServiceClient;
use flux_proto::v1::write_service_client::WriteServiceClient;
use flux_proto::v1::{self, *};
use flux_proto::{FRAME_BYTES, MAX_MESSAGE_BYTES};
use futures::StreamExt;
use tonic::transport::{Channel, Endpoint};

pub use flux_proto::v1::ChangesetInfo;

/// Bytes per upload stream before starting a new one.
const UPLOAD_BATCH_BYTES: usize = 32 * 1024 * 1024;
/// Concurrent upload streams.
const UPLOAD_STREAMS: usize = 4;
/// Max ids per batch request.
const ID_BATCH: usize = 2_000;

#[derive(Clone)]
pub struct Client {
    repo: String,
    objects: ObjectServiceClient<Channel>,
    repos: RepoServiceClient<Channel>,
    writes: WriteServiceClient<Channel>,
    admin: AdminServiceClient<Channel>,
}

/// Decoded view of a [`ChangesetInfo`].
#[derive(Debug, Clone)]
pub struct CsInfo {
    pub id: ChangesetId,
    pub generation: u64,
    pub root_tree: TreeId,
    pub changeset: Changeset,
    pub git_sha1: Option<[u8; 20]>,
}

impl TryFrom<ChangesetInfo> for CsInfo {
    type Error = anyhow::Error;
    fn try_from(i: ChangesetInfo) -> Result<Self> {
        let id = ChangesetId::from_slice(&i.id)?;
        let changeset = Changeset::decode_verified(&i.canonical, &id)?;
        Ok(CsInfo {
            id,
            generation: i.generation,
            root_tree: TreeId::from_slice(&i.root_tree)?,
            changeset,
            git_sha1: i.git_sha1.as_slice().try_into().ok(),
        })
    }
}

impl Client {
    /// Connect to `url` (e.g. `http://127.0.0.1:7878`) for repository `repo`.
    pub async fn connect(url: &str, repo: &str) -> Result<Self> {
        let channel = Endpoint::from_shared(url.to_string())?
            .tcp_nodelay(true)
            .http2_adaptive_window(true)
            .connect()
            .await
            .with_context(|| format!("connecting to {url}"))?;
        Ok(Self::from_channel(channel, repo))
    }

    pub fn from_channel(channel: Channel, repo: &str) -> Self {
        Self {
            repo: repo.to_string(),
            objects: ObjectServiceClient::new(channel.clone())
                .max_decoding_message_size(MAX_MESSAGE_BYTES)
                .max_encoding_message_size(MAX_MESSAGE_BYTES),
            repos: RepoServiceClient::new(channel.clone())
                .max_decoding_message_size(MAX_MESSAGE_BYTES)
                .max_encoding_message_size(MAX_MESSAGE_BYTES),
            writes: WriteServiceClient::new(channel.clone())
                .max_decoding_message_size(MAX_MESSAGE_BYTES)
                .max_encoding_message_size(MAX_MESSAGE_BYTES),
            admin: AdminServiceClient::new(channel),
        }
    }

    pub fn repo(&self) -> &str {
        &self.repo
    }

    // ───────────── admin ─────────────

    pub async fn create_repo(&self, name: &str) -> Result<()> {
        self.admin.clone().create_repo(CreateRepoRequest { name: name.into() }).await?;
        Ok(())
    }

    pub async fn list_repos(&self) -> Result<Vec<String>> {
        Ok(self.admin.clone().list_repos(ListReposRequest {}).await?.into_inner().repos.into_iter().map(|r| r.name).collect())
    }

    // ───────────── reads ─────────────

    pub async fn resolve(&self, rev: &str) -> Result<CsInfo> {
        let info = self
            .repos
            .clone()
            .resolve_rev(ResolveRevRequest { repo: self.repo.clone(), rev: rev.into() })
            .await
            .map_err(|s| anyhow!("{}", s.message()))?
            .into_inner();
        info.try_into()
    }

    pub async fn get_changesets(&self, ids: &[ChangesetId]) -> Result<Vec<CsInfo>> {
        let mut out = Vec::new();
        for chunk in ids.chunks(ID_BATCH) {
            let mut s = self
                .repos
                .clone()
                .get_changesets(GetChangesetsRequest {
                    repo: self.repo.clone(),
                    ids: chunk.iter().map(|i| i.as_bytes().to_vec()).collect(),
                })
                .await?
                .into_inner();
            while let Some(i) = s.next().await {
                out.push(i?.try_into()?);
            }
        }
        Ok(out)
    }

    pub async fn log(&self, rev: &str, limit: u32, first_parent: bool, path: Option<&[u8]>) -> Result<Vec<CsInfo>> {
        let mut s = self
            .repos
            .clone()
            .log(LogRequest {
                repo: self.repo.clone(),
                rev: rev.into(),
                limit,
                first_parent,
                path: path.map(<[u8]>::to_vec).unwrap_or_default(),
            })
            .await
            .map_err(|s| anyhow!("{}", s.message()))?
            .into_inner();
        let mut out = Vec::new();
        while let Some(i) = s.next().await {
            out.push(i?.try_into()?);
        }
        Ok(out)
    }

    pub async fn list_refs(&self, prefix: &[u8]) -> Result<Vec<RefEntry>> {
        Ok(self
            .repos
            .clone()
            .list_refs(ListRefsRequest { repo: self.repo.clone(), prefix: prefix.to_vec(), limit: 0 })
            .await?
            .into_inner()
            .refs)
    }

    pub async fn list_dir(&self, rev: &str, path: &[u8]) -> Result<ListDirResponse> {
        Ok(self
            .repos
            .clone()
            .list_dir(ListDirRequest { repo: self.repo.clone(), rev: rev.into(), path: path.to_vec() })
            .await
            .map_err(|s| anyhow!("{}", s.message()))?
            .into_inner())
    }

    pub async fn read_file(&self, rev: &str, path: &[u8]) -> Result<Vec<u8>> {
        let mut s = self
            .repos
            .clone()
            .read_file(ReadFileRequest { repo: self.repo.clone(), rev: rev.into(), path: path.to_vec() })
            .await
            .map_err(|s| anyhow!("{}", s.message()))?
            .into_inner();
        let mut buf = Vec::new();
        let mut id = None;
        while let Some(f) = s.next().await {
            let f = f?;
            id = Some(ContentId::from_slice(&f.content_id)?);
            buf.extend_from_slice(&f.data);
        }
        if let Some(id) = id
            && ContentId::hash(&buf) != id
        {
            bail!("content hash mismatch for {id}");
        }
        Ok(buf)
    }

    pub async fn diff(&self, old_rev: &str, new_rev: &str, prefix: &[u8]) -> Result<Vec<DiffEntry>> {
        let mut s = self
            .repos
            .clone()
            .diff(DiffRequest {
                repo: self.repo.clone(),
                old_rev: old_rev.into(),
                new_rev: new_rev.into(),
                path_prefix: prefix.to_vec(),
            })
            .await
            .map_err(|s| anyhow!("{}", s.message()))?
            .into_inner();
        let mut out = Vec::new();
        while let Some(e) = s.next().await {
            out.push(e?);
        }
        Ok(out)
    }

    /// Fetch trees (verified). With `depth > 0` the server also streams
    /// descendants, filtered by `sparse_prefixes` relative to `path_hint`.
    pub async fn get_trees(
        &self,
        ids: &[TreeId],
        depth: u32,
        sparse_prefixes: &[Vec<u8>],
        path_hint: &[u8],
    ) -> Result<Vec<(TreeId, Vec<u8>, Tree)>> {
        let mut out = Vec::new();
        for chunk in ids.chunks(ID_BATCH) {
            let mut s = self
                .objects
                .clone()
                .get_trees(GetTreesRequest {
                    repo: self.repo.clone(),
                    tree_ids: chunk.iter().map(|i| i.as_bytes().to_vec()).collect(),
                    depth,
                    sparse_prefixes: sparse_prefixes.to_vec(),
                    path_hint: path_hint.to_vec(),
                })
                .await?
                .into_inner();
            while let Some(t) = s.next().await {
                let t = t?;
                let id = TreeId::from_slice(&t.id)?;
                let tree = Tree::decode_verified(&t.canonical, &id)?;
                out.push((id, t.path, tree));
            }
        }
        Ok(out)
    }

    /// Fetch contents (verified), calling `sink` for each completed object.
    pub async fn get_contents(&self, ids: &[ContentId], mut sink: impl FnMut(ContentId, Vec<u8>) -> Result<()>) -> Result<()> {
        for chunk in ids.chunks(ID_BATCH) {
            let mut s = self
                .objects
                .clone()
                .get_contents(GetContentsRequest {
                    repo: self.repo.clone(),
                    content_ids: chunk.iter().map(|i| i.as_bytes().to_vec()).collect(),
                })
                .await?
                .into_inner();
            let mut partial: HashMap<ContentId, Vec<u8>> = HashMap::new();
            while let Some(f) = s.next().await {
                let f = f?;
                let id = ContentId::from_slice(&f.content_id)?;
                let buf = partial.entry(id).or_default();
                if f.offset as usize != buf.len() {
                    bail!("out-of-order frame for {id}");
                }
                buf.extend_from_slice(&f.data);
                if f.last {
                    let data = partial.remove(&id).unwrap();
                    if ContentId::hash(&data) != id {
                        bail!("content hash mismatch for {id}");
                    }
                    sink(id, data)?;
                }
            }
        }
        Ok(())
    }

    /// Map git commit SHA-1s to changeset ids.
    pub async fn git_lookup(&self, shas: &[[u8; 20]]) -> Result<Vec<Option<ChangesetId>>> {
        let mut out = Vec::with_capacity(shas.len());
        for chunk in shas.chunks(ID_BATCH) {
            let r = self
                .repos
                .clone()
                .git_lookup(GitLookupRequest { repo: self.repo.clone(), git_sha1s: chunk.iter().map(|s| s.to_vec()).collect() })
                .await?
                .into_inner();
            for id in r.changeset_ids {
                out.push(if id.is_empty() { None } else { Some(ChangesetId::from_slice(&id)?) });
            }
        }
        Ok(out)
    }

    // ───────────── writes ─────────────

    pub async fn has_contents(&self, ids: &[ContentId]) -> Result<Vec<bool>> {
        let mut out = Vec::with_capacity(ids.len());
        for chunk in ids.chunks(ID_BATCH) {
            out.extend(
                self.writes
                    .clone()
                    .has_contents(HasContentsRequest {
                        repo: self.repo.clone(),
                        content_ids: chunk.iter().map(|i| i.as_bytes().to_vec()).collect(),
                    })
                    .await?
                    .into_inner()
                    .present,
            );
        }
        Ok(out)
    }

    /// Upload contents the server doesn't already have. Returns their ids in
    /// input order.
    pub async fn upload(&self, contents: &[&[u8]]) -> Result<Vec<ContentId>> {
        let items: Vec<(ContentId, Bytes, Option<ContentId>)> =
            contents.iter().map(|c| (ContentId::hash(c), Bytes::copy_from_slice(c), None)).collect();
        let ids = items.iter().map(|i| i.0).collect();
        self.upload_known(items).await?;
        Ok(ids)
    }

    /// Upload `(id, data, delta_base)` triples (ids precomputed by the
    /// caller; the server re-verifies). `delta_base` names a previous version
    /// of the same path so the server can store a delta.
    pub async fn upload_known(&self, items: Vec<(ContentId, Bytes, Option<ContentId>)>) -> Result<()> {
        let ids: Vec<ContentId> = items.iter().map(|i| i.0).collect();
        let present = self.has_contents(&ids).await?;
        let mut seen = std::collections::HashSet::new();
        let mut batches: Vec<Vec<(ContentId, Bytes, Option<ContentId>)>> = vec![vec![]];
        let mut batch_bytes = 0;
        for (item, present) in items.into_iter().zip(present) {
            if present || !seen.insert(item.0) {
                continue;
            }
            batch_bytes += item.1.len();
            batches.last_mut().unwrap().push(item);
            if batch_bytes >= UPLOAD_BATCH_BYTES {
                batches.push(vec![]);
                batch_bytes = 0;
            }
        }
        let results: Vec<Result<()>> = futures::stream::iter(batches.into_iter().filter(|b| !b.is_empty()))
            .map(|b| self.upload_batch(b))
            .buffer_unordered(UPLOAD_STREAMS)
            .collect()
            .await;
        results.into_iter().collect()
    }

    async fn upload_batch(&self, batch: Vec<(ContentId, Bytes, Option<ContentId>)>) -> Result<()> {
        let mut frames = Vec::new();
        for (id, data, base) in &batch {
            let mut offset = 0;
            loop {
                let end = (offset + FRAME_BYTES).min(data.len());
                let first = offset == 0;
                frames.push(UploadFrame {
                    repo: if frames.is_empty() { self.repo.clone() } else { String::new() },
                    content_id: if first { id.as_bytes().to_vec() } else { vec![] },
                    data: data.slice(offset..end),
                    last: end == data.len(),
                    size: first.then_some(data.len() as u64),
                    delta_base: match (first, base) {
                        (true, Some(b)) => b.as_bytes().to_vec(),
                        _ => vec![],
                    },
                });
                if end == data.len() {
                    break;
                }
                offset = end;
            }
        }
        self.writes
            .clone()
            .upload_contents(tokio_stream::iter(frames))
            .await
            .map_err(|s| anyhow!("upload failed: {}", s.message()))?;
        Ok(())
    }

    pub async fn create_changeset(&self, cs: &Changeset) -> Result<CsInfo> {
        let info = self
            .writes
            .clone()
            .create_changeset(CreateChangesetRequest {
                repo: self.repo.clone(),
                changeset: Some(flux_proto::changeset_to_pb(cs)),
            })
            .await
            .map_err(|s| anyhow!("create changeset: {}", s.message()))?
            .into_inner();
        let info: CsInfo = info.try_into()?;
        if info.id != cs.id() {
            bail!("server computed changeset id {} but client expected {}", info.id, cs.id());
        }
        Ok(info)
    }

    /// Create many changesets in one call (in order; later ones may use
    /// earlier ones as parents).
    pub async fn create_changesets(&self, batch: &[Changeset]) -> Result<Vec<CsInfo>> {
        let infos = self
            .writes
            .clone()
            .create_changesets(CreateChangesetsRequest {
                repo: self.repo.clone(),
                changesets: batch.iter().map(flux_proto::changeset_to_pb).collect(),
            })
            .await
            .map_err(|s| anyhow!("create changesets: {}", s.message()))?
            .into_inner()
            .changesets;
        if infos.len() != batch.len() {
            bail!("server returned {} results for {} changesets", infos.len(), batch.len());
        }
        infos
            .into_iter()
            .zip(batch)
            .map(|(i, cs)| {
                let info: CsInfo = i.try_into()?;
                if info.id != cs.id() {
                    bail!("server computed changeset id {} but client expected {}", info.id, cs.id());
                }
                Ok(info)
            })
            .collect()
    }

    /// Compare-and-swap a ref. Returns `Ok(true)` if updated.
    pub async fn update_ref(
        &self,
        name: &str,
        expect: update_ref_request::Expect,
        new: Option<ChangesetId>,
        reason: &str,
    ) -> Result<bool> {
        let r = self
            .writes
            .clone()
            .update_ref(UpdateRefRequest {
                repo: self.repo.clone(),
                name: name.as_bytes().to_vec(),
                expect: Some(expect),
                new_target: new.map(|n| n.as_bytes().to_vec()).unwrap_or_default(),
                reason: reason.into(),
            })
            .await
            .map_err(|s| anyhow!("update ref: {}", s.message()))?
            .into_inner();
        Ok(r.updated)
    }

    pub async fn put_git_mapping(&self, pairs: &[(ChangesetId, [u8; 20])]) -> Result<()> {
        for chunk in pairs.chunks(ID_BATCH) {
            self.writes
                .clone()
                .put_git_mapping(PutGitMappingRequest {
                    repo: self.repo.clone(),
                    mappings: chunk
                        .iter()
                        .map(|(c, g)| v1::GitMapping { changeset_id: c.as_bytes().to_vec(), git_sha1: g.to_vec() })
                        .collect(),
                })
                .await?;
        }
        Ok(())
    }
}
