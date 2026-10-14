//! gRPC service implementations (SPEC §8).

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::Arc;

use flux_core::{ChangesetId, ChunkId, ContentId, EntryKind, RepoPath, TreeId};
use flux_derive::{diff_trees, lookup_path};
use flux_proto::v1::{self, *};
use flux_proto::{FRAME_BYTES, changeset_from_pb, content_id, cs_id, kind_to_pb, opt_repo_path, tree_id};
use flux_store::{CasOutcome, ContentLayout, RefExpect};
use futures::{Stream, StreamExt};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};

use crate::core::{Flux, FluxError};

type RpcStream<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send>>;

/// Run `f` in a task that feeds a bounded channel; returns the stream.
fn spawn_stream<T, F, Fut>(f: F) -> RpcStream<T>
where
    T: Send + 'static,
    F: FnOnce(mpsc::Sender<Result<T, Status>>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<(), Status>> + Send + 'static,
{
    let (tx, rx) = mpsc::channel(64);
    tokio::spawn(async move {
        let err_tx = tx.clone();
        if let Err(status) = f(tx).await {
            let _ = err_tx.send(Err(status)).await;
        }
    });
    Box::pin(ReceiverStream::new(rx))
}

fn entry_info(e: &flux_core::TreeEntry) -> TreeEntryInfo {
    let (kind, _) = match e.kind {
        EntryKind::File(k) => kind_to_pb(&k),
        EntryKind::Dir => (0, vec![]),
    };
    TreeEntryInfo {
        name: e.name.as_bytes().to_vec(),
        is_dir: e.is_dir(),
        kind,
        id: e.id.to_vec(),
        size: e.size,
        descendant_files: e.descendant_files,
    }
}

async fn send_content(
    flux: &Flux,
    id: ContentId,
    tx: &mpsc::Sender<Result<ContentFrame, Status>>,
) -> Result<bool, Status> {
    let layout = flux
        .blobs
        .content_layout(&id)
        .await
        .map_err(FluxError::from)?
        .ok_or_else(|| Status::not_found(format!("content {id} not found")))?;
    let parts: Vec<Option<ChunkId>> = match &layout {
        ContentLayout::Whole(_) => vec![None],
        ContentLayout::Chunked { chunks, .. } => chunks.iter().map(|c| Some(c.0)).collect(),
    };
    let total = match &layout {
        ContentLayout::Whole(b) => b.len() as u64,
        ContentLayout::Chunked { size, .. } => *size,
    };
    let mut offset = 0u64;
    for part in parts {
        let data = match (&layout, part) {
            (ContentLayout::Whole(b), _) => b.clone(),
            (_, Some(c)) => flux.blobs.get_chunk(&c).await.map_err(FluxError::from)?.ok_or_else(|| Status::internal(format!("chunk {c} missing")))?,
            _ => unreachable!(),
        };
        let mut pos = 0usize;
        loop {
            let end = (pos + FRAME_BYTES).min(data.len());
            let last = offset + end as u64 == total;
            let frame = ContentFrame { content_id: id.as_bytes().to_vec(), offset: offset + pos as u64, data: data.slice(pos..end), last };
            if tx.send(Ok(frame)).await.is_err() {
                return Ok(false); // client went away
            }
            if last {
                return Ok(true);
            }
            if end == data.len() {
                break;
            }
            pos = end;
        }
        offset += data.len() as u64;
    }
    Ok(true)
}

#[derive(Clone)]
pub struct Services {
    flux: Arc<Flux>,
}

impl Services {
    pub fn new(flux: Arc<Flux>) -> Self {
        Self { flux }
    }
}

// ───────────────────────────── ObjectService ─────────────────────────────

/// Should the directory at `path` be expanded given sparse `prefixes`?
fn sparse_allows(path: &[u8], prefixes: &[Vec<u8>]) -> bool {
    if prefixes.is_empty() || path.is_empty() {
        return true;
    }
    prefixes.iter().any(|p| {
        let under = path.starts_with(p) && (path.len() == p.len() || path[p.len()] == b'/');
        let ancestor = p.starts_with(path) && p.len() > path.len() && p[path.len()] == b'/';
        under || ancestor
    })
}

#[tonic::async_trait]
impl object_service_server::ObjectService for Services {
    type GetTreesStream = RpcStream<TreeObject>;
    type GetContentsStream = RpcStream<ContentFrame>;
    type GetContentMetaStream = RpcStream<ContentMetaResult>;

    async fn get_trees(&self, req: Request<GetTreesRequest>) -> Result<Response<Self::GetTreesStream>, Status> {
        let req = req.into_inner();
        let flux = self.flux.clone();
        flux.repo(&req.repo).await?;
        let mut queue: VecDeque<(TreeId, Vec<u8>, u32)> = VecDeque::new();
        for (i, raw) in req.tree_ids.iter().enumerate() {
            let path = if i == 0 { req.path_hint.clone() } else { vec![] };
            queue.push_back((tree_id(raw)?, path, 0));
        }
        let depth = req.depth.min(64);
        let prefixes = req.sparse_prefixes;
        Ok(Response::new(spawn_stream(move |tx| async move {
            while let Some((id, path, level)) = queue.pop_front() {
                let tree = flux
                    .blobs
                    .get_tree(&id)
                    .await
                    .map_err(FluxError::from)?
                    .ok_or_else(|| Status::not_found(format!("tree {id} not found")))?;
                if level < depth {
                    for e in tree.entries() {
                        if let Some(sub) = e.tree_id() {
                            let mut p = path.clone();
                            if !p.is_empty() {
                                p.push(b'/');
                            }
                            p.extend_from_slice(e.name.as_bytes());
                            if sparse_allows(&p, &prefixes) {
                                queue.push_back((sub, p, level + 1));
                            }
                        }
                    }
                }
                let obj = TreeObject { id: id.as_bytes().to_vec(), path, canonical: tree.encode().into() };
                if tx.send(Ok(obj)).await.is_err() {
                    break;
                }
            }
            Ok(())
        })))
    }

    async fn get_contents(&self, req: Request<GetContentsRequest>) -> Result<Response<Self::GetContentsStream>, Status> {
        let req = req.into_inner();
        let flux = self.flux.clone();
        flux.repo(&req.repo).await?;
        let ids: Vec<ContentId> = req.content_ids.iter().map(|b| content_id(b)).collect::<Result<_, _>>()?;
        Ok(Response::new(spawn_stream(move |tx| async move {
            for id in ids {
                if !send_content(&flux, id, &tx).await? {
                    break;
                }
            }
            Ok(())
        })))
    }

    async fn get_content_meta(
        &self,
        req: Request<GetContentMetaRequest>,
    ) -> Result<Response<Self::GetContentMetaStream>, Status> {
        let req = req.into_inner();
        self.flux.repo(&req.repo).await?;
        let ids: Vec<ContentId> = req.content_ids.iter().map(|b| content_id(b)).collect::<Result<_, _>>()?;
        let metas = self.flux.meta.get_content_meta(&ids).await.map_err(FluxError::from)?;
        let results: Vec<Result<ContentMetaResult, Status>> = ids
            .iter()
            .zip(metas)
            .map(|(id, m)| {
                Ok(match m {
                    Some(m) => meta_result(id, &m),
                    None => ContentMetaResult { content_id: id.as_bytes().to_vec(), found: false, ..Default::default() },
                })
            })
            .collect();
        Ok(Response::new(Box::pin(futures::stream::iter(results))))
    }
}

fn meta_result(id: &ContentId, m: &flux_store::ContentMeta) -> ContentMetaResult {
    ContentMetaResult {
        content_id: id.as_bytes().to_vec(),
        found: true,
        size: m.size,
        git_sha1: m.git_sha1.to_vec(),
        sha256: m.sha256.to_vec(),
        is_binary: m.is_binary,
    }
}

// ───────────────────────────── RepoService ─────────────────────────────

#[tonic::async_trait]
impl repo_service_server::RepoService for Services {
    type GetChangesetsStream = RpcStream<ChangesetInfo>;
    type LogStream = RpcStream<ChangesetInfo>;
    type ReadFileStream = RpcStream<ContentFrame>;
    type DiffStream = RpcStream<DiffEntry>;

    async fn resolve_rev(&self, req: Request<ResolveRevRequest>) -> Result<Response<ChangesetInfo>, Status> {
        let req = req.into_inner();
        let repo = self.flux.repo(&req.repo).await?;
        let id = self.flux.resolve_rev(repo, &req.rev).await?;
        let loaded = self.flux.load_one(repo, &id).await?;
        Ok(Response::new(self.flux.info(repo, &loaded).await?))
    }

    async fn list_refs(&self, req: Request<ListRefsRequest>) -> Result<Response<ListRefsResponse>, Status> {
        let req = req.into_inner();
        let repo = self.flux.repo(&req.repo).await?;
        let limit = if req.limit == 0 { 1000 } else { req.limit.min(10_000) };
        let refs = self.flux.meta.list_refs(repo, &req.prefix, limit).await.map_err(FluxError::from)?;
        Ok(Response::new(ListRefsResponse {
            refs: refs
                .into_iter()
                .map(|(name, v)| RefEntry { name, target: v.target.as_bytes().to_vec(), version: v.version })
                .collect(),
        }))
    }

    async fn get_changesets(
        &self,
        req: Request<GetChangesetsRequest>,
    ) -> Result<Response<Self::GetChangesetsStream>, Status> {
        let req = req.into_inner();
        let flux = self.flux.clone();
        let repo = flux.repo(&req.repo).await?;
        let ids: Vec<ChangesetId> = req.ids.iter().map(|b| cs_id(b)).collect::<Result<_, _>>()?;
        Ok(Response::new(spawn_stream(move |tx| async move {
            for loaded in flux.load(repo, &ids).await?.into_iter().flatten() {
                let info = flux.info(repo, &loaded).await?;
                if tx.send(Ok(info)).await.is_err() {
                    break;
                }
            }
            Ok(())
        })))
    }

    async fn log(&self, req: Request<LogRequest>) -> Result<Response<Self::LogStream>, Status> {
        let req = req.into_inner();
        let flux = self.flux.clone();
        let repo = flux.repo(&req.repo).await?;
        let start = flux.resolve_rev(repo, &req.rev).await?;
        let path = opt_repo_path(&req.path)?;
        let limit = if req.limit == 0 { usize::MAX } else { req.limit as usize };
        Ok(Response::new(spawn_stream(move |tx| async move {
            let mut batch = Vec::new();
            flux.log(repo, start, limit, req.first_parent, path.as_ref(), |cs| {
                batch.push(cs);
                true
            })
            .await?;
            for cs in batch {
                if tx.send(Ok(flux.info(repo, &cs).await?)).await.is_err() {
                    break;
                }
            }
            Ok(())
        })))
    }

    async fn list_dir(&self, req: Request<ListDirRequest>) -> Result<Response<ListDirResponse>, Status> {
        let req = req.into_inner();
        let repo = self.flux.repo(&req.repo).await?;
        let id = self.flux.resolve_rev(repo, &req.rev).await?;
        let root = self.flux.root_tree(repo, &id).await?;
        let dir = match opt_repo_path(&req.path)? {
            None => root,
            Some(p) => match lookup_path(&self.flux.blobs, root, &p).await.map_err(FluxError::from)? {
                Some(e) if e.is_dir() => e.tree_id().unwrap(),
                Some(_) => return Err(Status::failed_precondition(format!("{p} is a file"))),
                None => return Err(Status::not_found(format!("{p} not found"))),
            },
        };
        let tree = self.flux.blobs.get_tree(&dir).await.map_err(FluxError::from)?.ok_or_else(|| Status::internal("missing tree"))?;
        Ok(Response::new(ListDirResponse {
            tree_id: dir.as_bytes().to_vec(),
            entries: tree.entries().iter().map(entry_info).collect(),
        }))
    }

    async fn read_file(&self, req: Request<ReadFileRequest>) -> Result<Response<Self::ReadFileStream>, Status> {
        let req = req.into_inner();
        let repo = self.flux.repo(&req.repo).await?;
        let id = self.flux.resolve_rev(repo, &req.rev).await?;
        let path = flux_proto::repo_path(&req.path)?;
        let entry = self.flux.entry_at(repo, &id, &path).await?.ok_or_else(|| Status::not_found(format!("{path} not found")))?;
        let cid = entry.content_id().ok_or_else(|| Status::failed_precondition(format!("{path} is not a regular file")))?;
        let flux = self.flux.clone();
        Ok(Response::new(spawn_stream(move |tx| async move {
            send_content(&flux, cid, &tx).await?;
            Ok(())
        })))
    }

    async fn diff(&self, req: Request<DiffRequest>) -> Result<Response<Self::DiffStream>, Status> {
        let req = req.into_inner();
        let repo = self.flux.repo(&req.repo).await?;
        let new_id = self.flux.resolve_rev(repo, &req.new_rev).await?;
        let old_id = if req.old_rev.is_empty() {
            self.flux.load_one(repo, &new_id).await?.changeset.parents.first().copied()
        } else {
            Some(self.flux.resolve_rev(repo, &req.old_rev).await?)
        };
        let new_root = self.flux.root_tree(repo, &new_id).await?;
        let old_root = match old_id {
            Some(id) => Some(self.flux.root_tree(repo, &id).await?),
            None => None,
        };
        let prefix = opt_repo_path(&req.path_prefix)?;
        let include = move |p: &RepoPath, is_dir: bool| match &prefix {
            None => true,
            Some(pre) => p.starts_with(pre) || (is_dir && pre.starts_with(p)),
        };
        let entries = diff_trees(&self.flux.blobs, old_root, Some(new_root), &include).await.map_err(FluxError::from)?;
        let to_pb = |f: &flux_derive::TreeFile| {
            let (kind, _) = kind_to_pb(&f.kind);
            DiffFile { kind, content_id: f.content_id.map(|c| c.as_bytes().to_vec()).unwrap_or_default(), size: f.size }
        };
        let out: Vec<Result<DiffEntry, Status>> = entries
            .iter()
            .map(|e| Ok(DiffEntry { path: e.path.as_bytes().to_vec(), old: e.old.as_ref().map(to_pb), new: e.new.as_ref().map(to_pb) }))
            .collect();
        Ok(Response::new(Box::pin(futures::stream::iter(out))))
    }

    async fn git_lookup(&self, req: Request<GitLookupRequest>) -> Result<Response<GitLookupResponse>, Status> {
        let req = req.into_inner();
        let repo = self.flux.repo(&req.repo).await?;
        let shas: Vec<[u8; 20]> = req
            .git_sha1s
            .iter()
            .map(|s| s.as_slice().try_into().map_err(|_| Status::invalid_argument("git_sha1 must be 20 bytes")))
            .collect::<Result<_, _>>()?;
        let found = self.flux.meta.git_to_cs(repo, &shas).await.map_err(FluxError::from)?;
        Ok(Response::new(GitLookupResponse {
            changeset_ids: found.into_iter().map(|f| f.map(|c| c.as_bytes().to_vec()).unwrap_or_default()).collect(),
        }))
    }
}

// ───────────────────────────── WriteService ─────────────────────────────

/// Largest single content accepted (chunked contents are streamed, so this
/// bounds storage, not memory).
const MAX_CONTENT_BYTES: u64 = 64 << 30;
/// Small contents are hashed, compressed and stored in parallel batches.
const UPLOAD_BATCH_BYTES: usize = 64 << 20;

/// A content being received.
#[allow(clippy::large_enum_variant)] // one per upload stream
enum Incoming {
    Small { expected: ContentId, base: Option<ContentId>, buf: Vec<u8> },
    Large {
        expected: ContentId,
        size: u64,
        seen: u64,
        sink: flux_store::ChunkSink,
        id: flux_core::id::IdHasher<ContentId>,
        meta: flux_store::MetaHasher,
    },
}

impl Services {
    async fn store_batch(&self, batch: Vec<(ContentId, bytes::Bytes, Option<ContentId>)>) -> Result<Vec<ContentMetaResult>, Status> {
        if batch.is_empty() {
            return Ok(vec![]);
        }
        let expected: Vec<ContentId> = batch.iter().map(|b| b.0).collect();
        let stored = self.flux.put_contents(batch.into_iter().map(|(_, d, b)| (d, b)).collect()).await?;
        for ((id, _), want) in stored.iter().zip(&expected) {
            if id != want {
                return Err(Status::invalid_argument(format!("content hash mismatch: sent {want}, got {id}")));
            }
        }
        Ok(stored.iter().map(|(id, m)| meta_result(id, m)).collect())
    }
}

#[tonic::async_trait]
impl write_service_server::WriteService for Services {
    async fn has_contents(&self, req: Request<HasContentsRequest>) -> Result<Response<HasContentsResponse>, Status> {
        let req = req.into_inner();
        self.flux.repo(&req.repo).await?;
        let ids: Vec<ContentId> = req.content_ids.iter().map(|b| content_id(b)).collect::<Result<_, _>>()?;
        let metas = self.flux.meta.get_content_meta(&ids).await.map_err(FluxError::from)?;
        Ok(Response::new(HasContentsResponse { present: metas.iter().map(Option::is_some).collect() }))
    }

    async fn upload_contents(&self, req: Request<Streaming<UploadFrame>>) -> Result<Response<UploadResponse>, Status> {
        let mut stream = req.into_inner();
        let mut repo_checked = false;
        let mut stored = Vec::new();
        let mut batch: Vec<(ContentId, bytes::Bytes, Option<ContentId>)> = Vec::new();
        let mut batch_bytes = 0usize;
        let mut current: Option<Incoming> = None;
        while let Some(frame) = stream.next().await {
            let frame = frame?;
            if !repo_checked {
                self.flux.repo(&frame.repo).await?;
                repo_checked = true;
            }
            let cur = match current.as_mut() {
                Some(c) => c,
                None => {
                    let expected = content_id(&frame.content_id)?;
                    let base = if frame.delta_base.is_empty() { None } else { Some(content_id(&frame.delta_base)?) };
                    current.insert(match frame.size {
                        Some(size) if size as usize > flux_store::CHUNK_THRESHOLD => {
                            if size > MAX_CONTENT_BYTES {
                                return Err(Status::resource_exhausted("content too large"));
                            }
                            Incoming::Large {
                                expected,
                                size,
                                seen: 0,
                                sink: self.flux.blobs.chunk_sink(),
                                id: ContentId::hasher(),
                                meta: flux_store::MetaHasher::new(size),
                            }
                        }
                        _ => Incoming::Small { expected, base, buf: Vec::new() },
                    })
                }
            };
            match cur {
                Incoming::Small { buf, .. } => {
                    if buf.len() + frame.data.len() > flux_store::CHUNK_THRESHOLD {
                        return Err(Status::invalid_argument("content over 4 MiB must declare its size on the first frame"));
                    }
                    buf.extend_from_slice(&frame.data);
                }
                Incoming::Large { seen, sink, id, meta, size, .. } => {
                    *seen += frame.data.len() as u64;
                    if *seen > *size {
                        return Err(Status::invalid_argument("content longer than declared size"));
                    }
                    id.update(&frame.data);
                    meta.update(&frame.data);
                    sink.push(&frame.data).await.map_err(FluxError::from)?;
                }
            }
            if !frame.last {
                continue;
            }
            match current.take().unwrap() {
                Incoming::Small { expected, base, buf } => {
                    batch_bytes += buf.len();
                    batch.push((expected, bytes::Bytes::from(buf), base));
                    if batch_bytes >= UPLOAD_BATCH_BYTES {
                        stored.extend(self.store_batch(std::mem::take(&mut batch)).await?);
                        batch_bytes = 0;
                    }
                }
                Incoming::Large { expected, size, seen, sink, id, meta } => {
                    let actual = id.finish();
                    if seen != size {
                        return Err(Status::invalid_argument("content shorter than declared size"));
                    }
                    if actual != expected {
                        return Err(Status::invalid_argument(format!("content hash mismatch: sent {expected}, got {actual}")));
                    }
                    sink.finish(actual, size).await.map_err(FluxError::from)?;
                    let m = meta.finish();
                    self.flux.blobs.sync().await.map_err(FluxError::from)?;
                    self.flux.meta.put_content_meta(&[(actual, m)]).await.map_err(FluxError::from)?;
                    stored.push(meta_result(&actual, &m));
                }
            }
        }
        if current.is_some() {
            return Err(Status::invalid_argument("upload stream ended mid-content"));
        }
        stored.extend(self.store_batch(batch).await?);
        Ok(Response::new(UploadResponse { stored }))
    }

    async fn create_changeset(&self, req: Request<CreateChangesetRequest>) -> Result<Response<ChangesetInfo>, Status> {
        let req = req.into_inner();
        let repo = self.flux.repo(&req.repo).await?;
        let cs = changeset_from_pb(req.changeset.as_ref().ok_or_else(|| Status::invalid_argument("missing changeset"))?)?;
        Ok(Response::new(self.flux.create_changeset(repo, cs).await?))
    }

    async fn create_changesets(
        &self,
        req: Request<CreateChangesetsRequest>,
    ) -> Result<Response<CreateChangesetsResponse>, Status> {
        let req = req.into_inner();
        let repo = self.flux.repo(&req.repo).await?;
        let batch = req.changesets.iter().map(changeset_from_pb).collect::<Result<Vec<_>, _>>()?;
        Ok(Response::new(CreateChangesetsResponse { changesets: self.flux.create_changesets(repo, batch).await? }))
    }

    async fn update_ref(&self, req: Request<UpdateRefRequest>) -> Result<Response<UpdateRefResponse>, Status> {
        let req = req.into_inner();
        let repo = self.flux.repo(&req.repo).await?;
        let expect = match req.expect {
            Some(update_ref_request::Expect::Any(_)) => RefExpect::Any,
            Some(update_ref_request::Expect::Absent(_)) => RefExpect::Absent,
            Some(update_ref_request::Expect::Version(v)) => RefExpect::Version(v),
            Some(update_ref_request::Expect::Target(t)) => RefExpect::Target(cs_id(&t)?),
            None => return Err(Status::invalid_argument("an expectation is required (use any=true for unconditional)")),
        };
        let new = if req.new_target.is_empty() { None } else { Some(cs_id(&req.new_target)?) };
        let (outcome, current) = self.flux.update_ref(repo, &req.name, expect, new, req.reason).await?;
        let current = current.map(|c| RefEntry { name: req.name.clone(), target: c.target.as_bytes().to_vec(), version: c.version });
        Ok(Response::new(match outcome {
            CasOutcome::Updated { version } => UpdateRefResponse { updated: true, version, current: None },
            CasOutcome::Deleted => UpdateRefResponse { updated: true, version: 0, current: None },
            CasOutcome::Mismatch { .. } => UpdateRefResponse { updated: false, version: 0, current },
        }))
    }

    async fn put_git_mapping(&self, req: Request<PutGitMappingRequest>) -> Result<Response<PutGitMappingResponse>, Status> {
        let req = req.into_inner();
        let repo = self.flux.repo(&req.repo).await?;
        let mut entries = Vec::with_capacity(req.mappings.len());
        for m in &req.mappings {
            let sha: [u8; 20] = m.git_sha1.as_slice().try_into().map_err(|_| Status::invalid_argument("git_sha1 must be 20 bytes"))?;
            entries.push((cs_id(&m.changeset_id)?, sha));
        }
        let ids: Vec<ChangesetId> = entries.iter().map(|e| e.0).collect();
        if self.flux.meta.get_gens(repo, &ids).await.map_err(FluxError::from)?.iter().any(Option::is_none) {
            return Err(Status::failed_precondition("mapping references unknown changesets"));
        }
        self.flux.meta.git_map_put(repo, &entries).await.map_err(FluxError::from)?;
        Ok(Response::new(PutGitMappingResponse {}))
    }
}

// ───────────────────────────── AdminService ─────────────────────────────

#[tonic::async_trait]
impl admin_service_server::AdminService for Services {
    async fn create_repo(&self, req: Request<CreateRepoRequest>) -> Result<Response<v1::RepoInfo>, Status> {
        let name = req.into_inner().name;
        self.flux.create_repo(&name).await?;
        Ok(Response::new(v1::RepoInfo { name }))
    }

    async fn list_repos(&self, _req: Request<ListReposRequest>) -> Result<Response<ListReposResponse>, Status> {
        let repos = self.flux.meta.list_repos().await.map_err(FluxError::from)?;
        Ok(Response::new(ListReposResponse { repos: repos.into_iter().map(|r| v1::RepoInfo { name: r.name }).collect() }))
    }
}

