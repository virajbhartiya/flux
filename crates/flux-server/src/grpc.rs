//! gRPC service implementations (SPEC §8).

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::Arc;

use flux_core::{ChangesetId, ChunkId, ContentId, EntryKind, RepoPath, TreeId};
use flux_derive::{diff_trees, lookup_path};
use flux_proto::v1::{self, *};
use flux_proto::{FRAME_BYTES, changeset_from_pb, content_id, cs_id, kind_to_pb, opt_repo_path, tree_id};
use flux_store::{CasOutcome, ContentLayout, RefExpect, RepoId};
use futures::{Stream, StreamExt};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};

use crate::auth::Principal;
use crate::core::{Flux, FluxError};
use crate::rbac::Role;

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

/// Drop what `ids_only` callers already have.
fn trim_infos(mut infos: Vec<ChangesetInfo>, ids_only: bool) -> Vec<ChangesetInfo> {
    if ids_only {
        for i in &mut infos {
            i.canonical = Default::default();
            i.changeset = None;
        }
    }
    infos
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

async fn send_content(flux: &Flux, id: ContentId, tx: &mpsc::Sender<Result<ContentFrame, Status>>) -> Result<bool, Status> {
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
            (_, Some(c)) => flux
                .blobs
                .get_chunk(&c)
                .await
                .map_err(FluxError::from)?
                .ok_or_else(|| Status::internal(format!("chunk {c} missing")))?,
            _ => unreachable!(),
        };
        let mut pos = 0usize;
        loop {
            let end = (pos + FRAME_BYTES).min(data.len());
            let last = offset + end as u64 == total;
            let frame = ContentFrame {
                content_id: id.as_bytes().to_vec(),
                offset: offset + pos as u64,
                data: data.slice(pos..end),
                last,
            };
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

    /// The caller, from an `authorization: Bearer <token>` header.
    async fn who<T>(&self, req: &Request<T>) -> Result<Principal, Status> {
        let token = req
            .metadata()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(|v| v.strip_prefix("Bearer ").unwrap_or(v).to_string());
        Ok(self.flux.principal(token.as_deref()).await?)
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
        let principal = self.who(&req).await?;
        let req = req.into_inner();
        let flux = self.flux.clone();
        let repo = flux.repo(&req.repo).await?;
        flux.authorize(&principal, repo, Role::Read).await?;
        let mut queue: VecDeque<(TreeId, Vec<u8>, u32)> = VecDeque::new();
        for (i, raw) in req.tree_ids.iter().enumerate() {
            let path = if i == 0 { req.path_hint.clone() } else { vec![] };
            queue.push_back((tree_id(raw)?, path, 0));
        }
        let depth = req.depth.min(64);
        let prefixes = req.sparse_prefixes;
        let repo_id = flux.repo(&req.repo).await?;
        let acl = flux.repo_config(repo_id).await?;
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
                            if sparse_allows(&p, &prefixes) && acl.dir_visible(&principal, &p) {
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
        let principal = self.who(&req).await?;
        let req = req.into_inner();
        let flux = self.flux.clone();
        let repo = flux.repo(&req.repo).await?;
        flux.authorize(&principal, repo, Role::Read).await?;
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
        let principal = self.who(&req).await?;
        let req = req.into_inner();
        let repo = self.flux.repo(&req.repo).await?;
        self.flux.authorize(&principal, repo, Role::Read).await?;
        let ids: Vec<ContentId> = req.content_ids.iter().map(|b| content_id(b)).collect::<Result<_, _>>()?;
        let mut metas = self.flux.meta.get_content_meta(&ids).await.map_err(FluxError::from)?;
        // SHA-256 is not computed on write; fill it in for callers.
        for (id, m) in ids.iter().zip(metas.iter_mut()) {
            if let Some(m) = m
                && m.sha256 == [0; 32]
                && let Some(data) = self.flux.blobs.get_content_bytes(id).await.map_err(FluxError::from)?
            {
                use sha2::Digest as _;
                m.sha256 = sha2::Sha256::digest(&data).into();
            }
        }
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
        let principal = self.who(&req).await?;
        let req = req.into_inner();
        let repo = self.flux.repo(&req.repo).await?;
        self.flux.authorize(&principal, repo, Role::Read).await?;
        let id = self.flux.resolve_rev(repo, &req.rev).await?;
        let loaded = self.flux.load_one(repo, &id).await?;
        Ok(Response::new(self.flux.info(repo, &loaded).await?))
    }

    async fn list_refs(&self, req: Request<ListRefsRequest>) -> Result<Response<ListRefsResponse>, Status> {
        let principal = self.who(&req).await?;
        let req = req.into_inner();
        let repo = self.flux.repo(&req.repo).await?;
        self.flux.authorize(&principal, repo, Role::Read).await?;
        let limit = if req.limit == 0 { 1000 } else { req.limit.min(10_000) };
        let refs = self.flux.meta.list_refs(repo, &req.prefix, limit).await.map_err(FluxError::from)?;
        Ok(Response::new(ListRefsResponse {
            refs: refs
                .into_iter()
                .map(|(name, v)| RefEntry { name, target: v.target.as_bytes().to_vec(), version: v.version })
                .collect(),
        }))
    }

    async fn get_changesets(&self, req: Request<GetChangesetsRequest>) -> Result<Response<Self::GetChangesetsStream>, Status> {
        let principal = self.who(&req).await?;
        let req = req.into_inner();
        let flux = self.flux.clone();
        let repo = flux.repo(&req.repo).await?;
        flux.authorize(&principal, repo, Role::Read).await?;
        let ids: Vec<ChangesetId> = req.ids.iter().map(|b| cs_id(b)).collect::<Result<_, _>>()?;
        Ok(Response::new(spawn_stream(move |tx| async move {
            let loaded: Vec<_> = flux.load_arc(repo, &ids).await?.into_iter().flatten().collect();
            for info in flux.infos(repo, &loaded).await? {
                if tx.send(Ok(info)).await.is_err() {
                    break;
                }
            }
            Ok(())
        })))
    }

    async fn log(&self, req: Request<LogRequest>) -> Result<Response<Self::LogStream>, Status> {
        let principal = self.who(&req).await?;
        let req = req.into_inner();
        let flux = self.flux.clone();
        let repo = flux.repo(&req.repo).await?;
        flux.authorize(&principal, repo, Role::Read).await?;
        let start = flux.resolve_rev(repo, &req.rev).await?;
        let path = opt_repo_path(&req.path)?;
        let limit = if req.limit == 0 { usize::MAX } else { req.limit as usize };
        let mut hide = Vec::with_capacity(req.hide.len());
        for h in &req.hide {
            hide.push(flux.resolve_rev(repo, h).await?);
        }
        let skip = req.skip as usize;
        Ok(Response::new(spawn_stream(move |tx| async move {
            // Pages of 256 so huge logs stream without buffering everything.
            let mut done = 0usize;
            while done < limit {
                let page = (limit - done).min(256);
                let items = flux.log_page(repo, &start, &hide, path.as_ref(), req.first_parent, skip + done, page).await?;
                let n = items.len();
                for info in flux.infos(repo, &items).await? {
                    if tx.send(Ok(info)).await.is_err() {
                        return Ok(());
                    }
                }
                done += n;
                if n < page {
                    break;
                }
            }
            Ok(())
        })))
    }

    async fn list_dir(&self, req: Request<ListDirRequest>) -> Result<Response<ListDirResponse>, Status> {
        let principal = self.who(&req).await?;
        let req = req.into_inner();
        let repo = self.flux.repo(&req.repo).await?;
        self.flux.authorize(&principal, repo, Role::Read).await?;
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
        let tree =
            self.flux.blobs.get_tree(&dir).await.map_err(FluxError::from)?.ok_or_else(|| Status::internal("missing tree"))?;
        let acl = self.flux.repo_config(repo).await?;
        let base = req.path.clone();
        if !base.is_empty() && !acl.dir_visible(&principal, &base) {
            return Err(Status::not_found("not found"));
        }
        let child = |name: &[u8]| if base.is_empty() { name.to_vec() } else { [base.as_slice(), b"/", name].concat() };
        Ok(Response::new(ListDirResponse {
            tree_id: dir.as_bytes().to_vec(),
            entries: tree
                .entries()
                .iter()
                .filter(|e| acl.dir_visible(&principal, &child(e.name.as_bytes())))
                .map(entry_info)
                .collect(),
        }))
    }

    async fn read_file(&self, req: Request<ReadFileRequest>) -> Result<Response<Self::ReadFileStream>, Status> {
        let principal = self.who(&req).await?;
        let req = req.into_inner();
        let repo = self.flux.repo(&req.repo).await?;
        self.flux.authorize(&principal, repo, Role::Read).await?;
        let id = self.flux.resolve_rev(repo, &req.rev).await?;
        let path = flux_proto::repo_path(&req.path)?;
        self.flux.check_read_path(repo, &principal, path.as_bytes()).await?;
        let entry = self.flux.entry_at(repo, &id, &path).await?.ok_or_else(|| Status::not_found(format!("{path} not found")))?;
        let cid = entry.content_id().ok_or_else(|| Status::failed_precondition(format!("{path} is not a regular file")))?;
        let flux = self.flux.clone();
        Ok(Response::new(spawn_stream(move |tx| async move {
            send_content(&flux, cid, &tx).await?;
            Ok(())
        })))
    }

    async fn diff(&self, req: Request<DiffRequest>) -> Result<Response<Self::DiffStream>, Status> {
        let principal = self.who(&req).await?;
        let req = req.into_inner();
        let repo = self.flux.repo(&req.repo).await?;
        self.flux.authorize(&principal, repo, Role::Read).await?;
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
        let acl = self.flux.repo_config(repo).await?;
        let mut entries = diff_trees(&self.flux.blobs, old_root, Some(new_root), &include).await.map_err(FluxError::from)?;
        entries.retain(|e| acl.allows(&principal, e.path.as_bytes(), false));
        let to_pb = |f: &flux_derive::TreeFile| {
            let (kind, _) = kind_to_pb(&f.kind);
            DiffFile { kind, content_id: f.content_id.map(|c| c.as_bytes().to_vec()).unwrap_or_default(), size: f.size }
        };
        let out: Vec<Result<DiffEntry, Status>> = entries
            .iter()
            .map(|e| {
                Ok(DiffEntry { path: e.path.as_bytes().to_vec(), old: e.old.as_ref().map(to_pb), new: e.new.as_ref().map(to_pb) })
            })
            .collect();
        Ok(Response::new(Box::pin(futures::stream::iter(out))))
    }

    async fn git_lookup(&self, req: Request<GitLookupRequest>) -> Result<Response<GitLookupResponse>, Status> {
        let principal = self.who(&req).await?;
        let req = req.into_inner();
        let repo = self.flux.repo(&req.repo).await?;
        self.flux.authorize(&principal, repo, Role::Read).await?;
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
    type SearchStream = RpcStream<SearchMatch>;

    async fn blame(&self, req: Request<BlameRequest>) -> Result<Response<BlameResponse>, Status> {
        let principal = self.who(&req).await?;
        let req = req.into_inner();
        let repo = self.flux.repo(&req.repo).await?;
        self.flux.authorize(&principal, repo, Role::Read).await?;
        let id = self.flux.resolve_rev(repo, &req.rev).await?;
        let path = flux_proto::repo_path(&req.path)?;
        self.flux.check_read_path(repo, &principal, path.as_bytes()).await?;
        let blame = self.flux.blame(repo, &id, &path).await?;
        let mut ids: Vec<ChangesetId> = blame.ranges.iter().map(|r| r.2).collect();
        ids.sort();
        ids.dedup();
        let loaded: Vec<_> = self.flux.load_arc(repo, &ids).await?.into_iter().flatten().collect();
        Ok(Response::new(BlameResponse {
            ranges: blame
                .ranges
                .iter()
                .map(|(start, count, cs)| BlameRange {
                    start_line: *start,
                    line_count: *count,
                    changeset_id: cs.as_bytes().to_vec(),
                })
                .collect(),
            changesets: self.flux.infos(repo, &loaded).await?,
        }))
    }

    async fn ancestry(&self, req: Request<AncestryRequest>) -> Result<Response<AncestryResponse>, Status> {
        let principal = self.who(&req).await?;
        let req = req.into_inner();
        let repo = self.flux.repo(&req.repo).await?;
        self.flux.authorize(&principal, repo, Role::Read).await?;
        let a = self.flux.resolve_rev(repo, &req.a).await?;
        let b = self.flux.resolve_rev(repo, &req.b).await?;
        let (a_ahead, b_ahead) = self.flux.ahead_behind(repo, &a, &b).await?;
        Ok(Response::new(AncestryResponse {
            a_is_ancestor_of_b: self.flux.is_ancestor(repo, &a, &b).await?,
            b_is_ancestor_of_a: self.flux.is_ancestor(repo, &b, &a).await?,
            merge_base: self.flux.merge_base(repo, &a, &b).await?.map(|m| m.as_bytes().to_vec()).unwrap_or_default(),
            a_ahead: a_ahead as u64,
            b_ahead: b_ahead as u64,
        }))
    }

    async fn search(&self, req: Request<SearchRequest>) -> Result<Response<Self::SearchStream>, Status> {
        let principal = self.who(&req).await?;
        let req = req.into_inner();
        let repo = self.flux.repo(&req.repo).await?;
        self.flux.authorize(&principal, repo, Role::Read).await?;
        let id = self.flux.resolve_rev(repo, if req.rev.is_empty() { "main" } else { &req.rev }).await?;
        let root = self.flux.root_tree(repo, &id).await?;
        let prefix = opt_repo_path(&req.path_prefix)?;
        let limit = if req.limit == 0 { 500 } else { req.limit.min(10_000) as usize };
        let result = self.flux.search(repo, root, &req.pattern, req.regex, req.case_insensitive, prefix.as_ref(), limit).await?;
        let acl = self.flux.repo_config(repo).await?;
        let out: Vec<Result<SearchMatch, Status>> = result
            .hits
            .into_iter()
            .filter(|h| acl.allows(&principal, h.path.as_bytes(), false))
            .map(|h| Ok(SearchMatch { path: h.path.into_bytes(), line_number: h.line_number, line: h.line }))
            .collect();
        Ok(Response::new(Box::pin(futures::stream::iter(out))))
    }

    async fn find_files(&self, req: Request<FindFilesRequest>) -> Result<Response<FindFilesResponse>, Status> {
        let principal = self.who(&req).await?;
        let req = req.into_inner();
        let repo = self.flux.repo(&req.repo).await?;
        self.flux.authorize(&principal, repo, Role::Read).await?;
        let id = self.flux.resolve_rev(repo, if req.rev.is_empty() { "main" } else { &req.rev }).await?;
        let root = self.flux.root_tree(repo, &id).await?;
        let limit = if req.limit == 0 { 100 } else { req.limit.min(100_000) as usize };
        let paths = self.flux.find_files(root, &req.query, limit).await?;
        let acl = self.flux.repo_config(repo).await?;
        Ok(Response::new(FindFilesResponse {
            paths: paths.into_iter().filter(|p| acl.allows(&principal, p.as_bytes(), false)).map(RepoPath::into_bytes).collect(),
        }))
    }

    async fn last_modified(&self, req: Request<LastModifiedRequest>) -> Result<Response<LastModifiedResponse>, Status> {
        let principal = self.who(&req).await?;
        let req = req.into_inner();
        let repo = self.flux.repo(&req.repo).await?;
        self.flux.authorize(&principal, repo, Role::Read).await?;
        let id = self.flux.resolve_rev(repo, &req.rev).await?;
        let path = opt_repo_path(&req.path)?;
        let root = self.flux.lastmod(repo, &id).await?;
        let node = flux_derive::lastmod_dir(&self.flux.blobs, root, path.as_ref())
            .await
            .map_err(FluxError::from)?
            .ok_or_else(|| Status::not_found("no such directory"))?;
        let mut ids: Vec<ChangesetId> = node.entries.iter().map(|e| e.cs).collect();
        ids.sort();
        ids.dedup();
        let loaded: Vec<_> = self.flux.load_arc(repo, &ids).await?.into_iter().flatten().collect();
        Ok(Response::new(LastModifiedResponse {
            entries: node
                .entries
                .iter()
                .map(|e| LastModifiedEntry { name: e.name.as_bytes().to_vec(), changeset_id: e.cs.as_bytes().to_vec() })
                .collect(),
            changesets: self.flux.infos(repo, &loaded).await?,
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
    Small {
        expected: ContentId,
        base: Option<ContentId>,
        buf: Vec<u8>,
    },
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
    async fn store_batch(
        &self,
        batch: Vec<(ContentId, bytes::Bytes, Option<ContentId>)>,
    ) -> Result<Vec<ContentMetaResult>, Status> {
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

    /// Decode a create request and check path rules for it.
    async fn create_batch(
        &self,
        repo: RepoId,
        principal: &Principal,
        req: &CreateChangesetsRequest,
    ) -> Result<(Vec<flux_core::Changeset>, Vec<[u8; 20]>), Status> {
        let batch = req.changesets.iter().map(changeset_from_pb).collect::<Result<Vec<_>, _>>()?;
        let paths: Vec<Vec<u8>> = batch.iter().flat_map(|c| c.file_changes.keys().map(|p| p.as_bytes().to_vec())).collect();
        self.flux.check_write_paths(repo, principal, paths).await?;
        let shas = req
            .git_sha1s
            .iter()
            .map(|s| s.as_slice().try_into().map_err(|_| Status::invalid_argument("git_sha1s must be 20 bytes")))
            .collect::<Result<Vec<[u8; 20]>, _>>()?;
        Ok((batch, shas))
    }
}

#[tonic::async_trait]
impl write_service_server::WriteService for Services {
    async fn has_contents(&self, req: Request<HasContentsRequest>) -> Result<Response<HasContentsResponse>, Status> {
        let principal = self.who(&req).await?;
        let req = req.into_inner();
        let repo = self.flux.repo(&req.repo).await?;
        self.flux.authorize(&principal, repo, Role::Write).await?;
        let ids: Vec<ContentId> = req.content_ids.iter().map(|b| content_id(b)).collect::<Result<_, _>>()?;
        let metas = self.flux.meta.get_content_meta(&ids).await.map_err(FluxError::from)?;
        Ok(Response::new(HasContentsResponse { present: metas.iter().map(Option::is_some).collect() }))
    }

    async fn upload_contents(&self, req: Request<Streaming<UploadFrame>>) -> Result<Response<UploadResponse>, Status> {
        let principal = self.who(&req).await?;
        let mut stream = req.into_inner();
        let mut repo_checked = false;
        let mut stored = Vec::new();
        let mut batch: Vec<(ContentId, bytes::Bytes, Option<ContentId>)> = Vec::new();
        let mut batch_bytes = 0usize;
        let mut current: Option<Incoming> = None;
        while let Some(frame) = stream.next().await {
            let frame = frame?;
            if !repo_checked {
                let repo = self.flux.repo(&frame.repo).await?;
                self.flux.authorize(&principal, repo, Role::Write).await?;
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
        let principal = self.who(&req).await?;
        let req = req.into_inner();
        let repo = self.flux.repo(&req.repo).await?;
        self.flux.authorize(&principal, repo, Role::Write).await?;
        let cs = changeset_from_pb(req.changeset.as_ref().ok_or_else(|| Status::invalid_argument("missing changeset"))?)?;
        let paths: Vec<Vec<u8>> = cs.file_changes.keys().map(|p| p.as_bytes().to_vec()).collect();
        self.flux.check_write_paths(repo, &principal, paths).await?;
        Ok(Response::new(self.flux.create_changeset(repo, cs).await?))
    }

    async fn create_changesets(
        &self,
        req: Request<CreateChangesetsRequest>,
    ) -> Result<Response<CreateChangesetsResponse>, Status> {
        let principal = self.who(&req).await?;
        let req = req.into_inner();
        let repo = self.flux.repo(&req.repo).await?;
        self.flux.authorize(&principal, repo, Role::Write).await?;
        let (batch, shas) = self.create_batch(repo, &principal, &req).await?;
        let infos = self.flux.create_changesets_mapped(repo, batch, &shas).await?;
        Ok(Response::new(CreateChangesetsResponse { changesets: trim_infos(infos, req.ids_only) }))
    }

    type CreateChangesetsStreamStream = RpcStream<CreateChangesetsResponse>;

    async fn create_changesets_stream(
        &self,
        req: Request<Streaming<CreateChangesetsRequest>>,
    ) -> Result<Response<Self::CreateChangesetsStreamStream>, Status> {
        let principal = self.who(&req).await?;
        let mut input = req.into_inner();
        let this = self.clone();
        Ok(Response::new(spawn_stream(move |tx| async move {
            // This task prepares batches in order; a committer task commits
            // them in the same order, one batch behind.
            let (ctx, mut crx) = mpsc::channel::<(RepoId, crate::core::PreparedBatch, bool)>(1);
            let in_flight = Arc::new(std::sync::Mutex::new(crate::core::InFlight::new()));
            let committer = {
                let (flux, tx, in_flight) = (this.flux.clone(), tx.clone(), in_flight.clone());
                tokio::spawn(async move {
                    while let Some((repo, prepared, ids_only)) = crx.recv().await {
                        let ids: Vec<ChangesetId> = prepared.prepared().map(|p| p.0).collect();
                        match flux.commit_changesets(repo, prepared).await {
                            Ok(infos) => {
                                {
                                    let mut f = in_flight.lock().unwrap();
                                    for id in &ids {
                                        f.remove(id);
                                    }
                                }
                                let r = CreateChangesetsResponse { changesets: trim_infos(infos, ids_only) };
                                if tx.send(Ok(r)).await.is_err() {
                                    return false;
                                }
                            }
                            Err(e) => {
                                let _ = tx.send(Err(e.into())).await;
                                return false;
                            }
                        }
                    }
                    true
                })
            };
            let mut repo_of: Option<(String, RepoId)> = None;
            let result: Result<(), Status> = async {
                while let Some(req) = input.message().await? {
                    let repo = match &repo_of {
                        Some((name, id)) if *name == req.repo => *id,
                        Some(_) => return Err(Status::invalid_argument("every request in a stream must name the same repo")),
                        None => {
                            let id = this.flux.repo(&req.repo).await?;
                            this.flux.authorize(&principal, id, Role::Write).await?;
                            repo_of = Some((req.repo.clone(), id));
                            id
                        }
                    };
                    let (batch, shas) = this.create_batch(repo, &principal, &req).await?;
                    let snapshot = in_flight.lock().unwrap().clone();
                    let prepared = this.flux.prepare_changesets(repo, batch, &shas, &snapshot).await?;
                    in_flight.lock().unwrap().extend(prepared.prepared().map(|(id, g, t)| (id, (g, t))));
                    if ctx.send((repo, prepared, req.ids_only)).await.is_err() {
                        break; // the committer failed and reported it
                    }
                }
                Ok(())
            }
            .await;
            drop(ctx);
            // Commit (and answer) everything prepared before any error.
            let committed = committer.await.map_err(|e| Status::internal(e.to_string()))?;
            match result {
                Err(e) if committed => Err(e),
                _ => Ok(()),
            }
        })))
    }

    async fn update_ref(&self, req: Request<UpdateRefRequest>) -> Result<Response<UpdateRefResponse>, Status> {
        let principal = self.who(&req).await?;
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
        let (outcome, current) = self.flux.update_ref_as(&principal, repo, &req.name, expect, new, req.reason).await?;
        let current =
            current.map(|c| RefEntry { name: req.name.clone(), target: c.target.as_bytes().to_vec(), version: c.version });
        Ok(Response::new(match outcome {
            CasOutcome::Updated { version } => UpdateRefResponse { updated: true, version, current: None },
            CasOutcome::Deleted => UpdateRefResponse { updated: true, version: 0, current: None },
            CasOutcome::Mismatch { .. } => UpdateRefResponse { updated: false, version: 0, current },
        }))
    }

    async fn put_git_mapping(&self, req: Request<PutGitMappingRequest>) -> Result<Response<PutGitMappingResponse>, Status> {
        let principal = self.who(&req).await?;
        let req = req.into_inner();
        let repo = self.flux.repo(&req.repo).await?;
        self.flux.authorize(&principal, repo, Role::Write).await?;
        let mut entries = Vec::with_capacity(req.mappings.len());
        for m in &req.mappings {
            let sha: [u8; 20] =
                m.git_sha1.as_slice().try_into().map_err(|_| Status::invalid_argument("git_sha1 must be 20 bytes"))?;
            entries.push((cs_id(&m.changeset_id)?, sha));
        }
        let ids: Vec<ChangesetId> = entries.iter().map(|e| e.0).collect();
        if self.flux.meta.get_gens(repo, &ids).await.map_err(FluxError::from)?.iter().any(Option::is_none) {
            return Err(Status::failed_precondition("mapping references unknown changesets"));
        }
        self.flux.meta.git_map_put(repo, &entries).await.map_err(FluxError::from)?;
        Ok(Response::new(PutGitMappingResponse {}))
    }
    async fn land(&self, req: Request<LandRequest>) -> Result<Response<LandResponse>, Status> {
        let principal = self.who(&req).await?;
        let req = req.into_inner();
        let repo = self.flux.repo(&req.repo).await?;
        self.flux.authorize(&principal, repo, Role::Write).await?;
        let top = self.flux.resolve_rev(repo, &req.rev).await?;
        let target = if req.target_ref.is_empty() { "main".to_string() } else { req.target_ref };
        let reason = if req.reason.is_empty() { "flux land".to_string() } else { req.reason };
        let r = self.flux.land_as(&principal, repo, top, target.as_bytes(), &reason).await?;
        Ok(Response::new(land_pb(&r)))
    }
}

fn land_pb(r: &crate::land::LandResult) -> LandResponse {
    LandResponse {
        new_head: r.head.as_bytes().to_vec(),
        rebased: r.rebased.iter().map(|c| c.as_bytes().to_vec()).collect(),
        fast_forward: r.fast_forward,
        merged_files: r.merged_files,
    }
}

// ───────────────────────────── AdminService ─────────────────────────────

#[tonic::async_trait]
impl admin_service_server::AdminService for Services {
    async fn create_repo(&self, req: Request<CreateRepoRequest>) -> Result<Response<v1::RepoInfo>, Status> {
        let principal = self.who(&req).await?;
        let req = req.into_inner();
        let visibility = if req.visibility.is_empty() { None } else { Some(req.visibility.as_str()) };
        self.flux.create_repo_as(&principal, &req.name, visibility).await?;
        Ok(Response::new(v1::RepoInfo { name: req.name }))
    }

    async fn list_repos(&self, req: Request<ListReposRequest>) -> Result<Response<ListReposResponse>, Status> {
        let principal = self.who(&req).await?;
        let repos = self.flux.readable_repos(&principal).await?;
        Ok(Response::new(ListReposResponse { repos: repos.into_iter().map(|r| v1::RepoInfo { name: r.name }).collect() }))
    }
}

// ───────────────────────────── ReviewService ─────────────────────────────

fn review_pb(r: &flux_store::Review) -> v1::Review {
    v1::Review {
        number: r.number,
        title: r.title.clone(),
        body: r.body.clone(),
        author: r.author.clone(),
        head: r.head.as_bytes().to_vec(),
        target: String::from_utf8_lossy(&r.target).into_owned(),
        state: r.state.clone(),
        landed: r.landed.map(|l| l.as_bytes().to_vec()).unwrap_or_default(),
        created_ms: r.created_ms,
        updated_ms: r.updated_ms,
    }
}

#[tonic::async_trait]
impl review_service_server::ReviewService for Services {
    async fn create_review(&self, req: Request<CreateReviewRequest>) -> Result<Response<v1::Review>, Status> {
        let principal = self.who(&req).await?;
        let req = req.into_inner();
        let repo = self.flux.repo(&req.repo).await?;
        self.flux.authorize(&principal, repo, Role::Write).await?;
        let head = self.flux.resolve_rev(repo, &req.head_rev).await?;
        let target = if req.target.is_empty() { "main".to_string() } else { req.target };
        let r = self.flux.create_review(repo, &req.title, &req.body, principal.name(), head, target.as_bytes()).await?;
        Ok(Response::new(review_pb(&r)))
    }

    async fn list_reviews(&self, req: Request<ListReviewsRequest>) -> Result<Response<ListReviewsResponse>, Status> {
        let principal = self.who(&req).await?;
        let req = req.into_inner();
        let repo = self.flux.repo(&req.repo).await?;
        self.flux.authorize(&principal, repo, Role::Read).await?;
        let state = if req.state.is_empty() { None } else { Some(req.state.as_str()) };
        let limit = if req.limit == 0 { 100 } else { req.limit.min(1000) };
        let rs = self.flux.meta.list_reviews(repo, state, limit).await.map_err(FluxError::from)?;
        Ok(Response::new(ListReviewsResponse { reviews: rs.iter().map(review_pb).collect() }))
    }

    async fn get_review(&self, req: Request<GetReviewRequest>) -> Result<Response<ReviewDetail>, Status> {
        let principal = self.who(&req).await?;
        let req = req.into_inner();
        let repo = self.flux.repo(&req.repo).await?;
        self.flux.authorize(&principal, repo, Role::Read).await?;
        let d = self.flux.review_detail(repo, req.number).await?;
        Ok(Response::new(ReviewDetail {
            review: Some(review_pb(&d.review)),
            events: d
                .events
                .iter()
                .map(|e| v1::ReviewEvent {
                    kind: e.kind.clone(),
                    author: e.author.clone(),
                    body: e.body.clone(),
                    changeset_id: e.cs.map(|c| c.as_bytes().to_vec()).unwrap_or_default(),
                    path: e.path.clone().unwrap_or_default(),
                    line: e.line.unwrap_or(0),
                    ts_ms: e.ts_ms,
                })
                .collect(),
            stack: self.flux.infos(repo, &d.stack).await?,
        }))
    }

    async fn update_review(&self, req: Request<UpdateReviewRequest>) -> Result<Response<v1::Review>, Status> {
        let principal = self.who(&req).await?;
        let req = req.into_inner();
        let repo = self.flux.repo(&req.repo).await?;
        self.flux.authorize(&principal, repo, Role::Write).await?;
        let head = if req.head_rev.is_empty() { None } else { Some(self.flux.resolve_rev(repo, &req.head_rev).await?) };
        let r = self.flux.update_review(repo, req.number, &req.action, principal.name(), &req.body, head).await?;
        Ok(Response::new(review_pb(&r)))
    }

    async fn land_review(&self, req: Request<LandReviewRequest>) -> Result<Response<LandResponse>, Status> {
        let principal = self.who(&req).await?;
        let req = req.into_inner();
        let repo = self.flux.repo(&req.repo).await?;
        let r = self.flux.land_review_as(&principal, repo, req.number, Default::default()).await?;
        Ok(Response::new(land_pb(&r)))
    }
}

// ───────────────────────────── EventService ─────────────────────────────

pub fn event_pb(e: &crate::core::Event) -> RepoEvent {
    use crate::core::Event;
    match e {
        Event::RefMoved { repo, name, old, new, actor } => RepoEvent {
            repo: repo.clone(),
            kind: "ref".into(),
            ref_name: name.clone(),
            old: old.map(|c| c.as_bytes().to_vec()).unwrap_or_default(),
            new: new.map(|c| c.as_bytes().to_vec()).unwrap_or_default(),
            actor: actor.clone(),
            ..Default::default()
        },
        Event::Changesets { repo, count, last } => RepoEvent {
            repo: repo.clone(),
            kind: "changesets".into(),
            new: last.as_bytes().to_vec(),
            count: *count as u64,
            ..Default::default()
        },
        Event::Review { repo, number, kind, actor } => RepoEvent {
            repo: repo.clone(),
            kind: "review".into(),
            review: *number,
            detail: kind.clone(),
            actor: actor.clone(),
            ..Default::default()
        },
        Event::Issue { repo, number, action, actor } => RepoEvent {
            repo: repo.clone(),
            kind: "issue".into(),
            review: *number,
            detail: action.clone(),
            actor: actor.clone(),
            ..Default::default()
        },
        Event::Comment { repo, number, on, actor, .. } => RepoEvent {
            repo: repo.clone(),
            kind: "comment".into(),
            review: *number,
            detail: on.to_string(),
            actor: actor.clone(),
            ..Default::default()
        },
        Event::Status { repo, cs, context, state, actor } => RepoEvent {
            repo: repo.clone(),
            kind: "status".into(),
            new: cs.as_bytes().to_vec(),
            detail: format!("{context}={state}"),
            actor: actor.clone(),
            ..Default::default()
        },
        Event::Release { repo, id, action, actor } => RepoEvent {
            repo: repo.clone(),
            kind: "release".into(),
            count: *id as u64,
            detail: action.clone(),
            actor: actor.clone(),
            ..Default::default()
        },
        Event::Ci { repo, run, state } => {
            RepoEvent { repo: repo.clone(), kind: "ci".into(), count: *run as u64, detail: state.clone(), ..Default::default() }
        }
    }
}

#[tonic::async_trait]
impl event_service_server::EventService for Services {
    type WatchStream = RpcStream<RepoEvent>;

    async fn watch(&self, req: Request<WatchRequest>) -> Result<Response<Self::WatchStream>, Status> {
        let principal = self.who(&req).await?;
        let req = req.into_inner();
        if !req.repo.is_empty() {
            let repo = self.flux.repo(&req.repo).await?;
            self.flux.authorize(&principal, repo, Role::Read).await?;
        }
        let mut rx = self.flux.subscribe();
        let flux = self.flux.clone();
        Ok(Response::new(spawn_stream(move |tx| async move {
            loop {
                match rx.recv().await {
                    Ok(e) => {
                        // Only events from repositories this caller may read.
                        let visible = match flux.repo(e.repo()).await {
                            Ok(id) => flux.repo_role(&principal, id).await.is_ok_and(|r| r >= Role::Read),
                            Err(_) => false,
                        };
                        if visible && (req.repo.is_empty() || e.repo() == req.repo) && tx.send(Ok(event_pb(&e))).await.is_err() {
                            return Ok(());
                        }
                    }
                    // A slow watcher missed events; tell it to resync.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        let lag = RepoEvent { kind: "lagged".into(), count: n, ..Default::default() };
                        if tx.send(Ok(lag)).await.is_err() {
                            return Ok(());
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return Ok(()),
                }
            }
        })))
    }
}

// ───────────────────────────── AuthService ─────────────────────────────

#[tonic::async_trait]
impl auth_service_server::AuthService for Services {
    async fn login(&self, req: Request<LoginRequest>) -> Result<Response<LoginResponse>, Status> {
        let client = req.remote_addr().map(|a| a.ip().to_string()).unwrap_or_else(|| "grpc".into());
        let req = req.into_inner();
        // Check the password (throttled), then mint an API token rather
        // than keeping the session.
        let otp = Some(req.otp.as_str()).filter(|o| !o.is_empty());
        let (user, session) = self.flux.password_login_2fa(&req.login, &req.password, otp, &client).await?;
        let principal = self.flux.principal(Some(&session)).await?;
        let scopes = if req.scopes.is_empty() { "all" } else { req.scopes.as_str() };
        let label = if req.label.is_empty() { "flux CLI" } else { req.label.as_str() };
        let expires = (req.expires_days > 0).then_some(req.expires_days as i64);
        let token = self.flux.create_api_token(&principal, label, scopes, expires).await;
        let _ = self.flux.logout(&session).await;
        Ok(Response::new(LoginResponse { token: token?, user: user.name }))
    }

    async fn who_am_i(&self, req: Request<WhoAmIRequest>) -> Result<Response<WhoAmIResponse>, Status> {
        let p = self.who(&req).await?;
        Ok(Response::new(WhoAmIResponse {
            user: p.user.as_ref().map(|u| u.name.clone()).unwrap_or_default(),
            is_admin: p.is_admin(),
            can_write: p.can_write(),
            auth_mode: p.mode.as_str().into(),
        }))
    }
}
