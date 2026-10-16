//! Rust client for flux. Every object fetched is verified against its id, so
//! callers can trust content regardless of which tier served it.

pub mod daemon;

use std::collections::HashMap;

use anyhow::{Context, Result, anyhow, bail};
use bytes::Bytes;
use flux_core::{Changeset, ChangesetId, ContentId, Tree, TreeId};
use flux_proto::v1::admin_service_client::AdminServiceClient;
use flux_proto::v1::auth_service_client::AuthServiceClient;
use flux_proto::v1::event_service_client::EventServiceClient;
use flux_proto::v1::object_service_client::ObjectServiceClient;
use flux_proto::v1::repo_service_client::RepoServiceClient;
use flux_proto::v1::review_service_client::ReviewServiceClient;
use flux_proto::v1::write_service_client::WriteServiceClient;
use flux_proto::v1::{self, *};
use flux_proto::{FRAME_BYTES, MAX_MESSAGE_BYTES};
use futures::StreamExt;
use tonic::metadata::MetadataValue;
use tonic::service::interceptor::InterceptedService;
use tonic::transport::{Channel, Endpoint};

/// Adds `authorization: Bearer <token>` to every request when set.
#[derive(Clone)]
pub struct Auth(Option<MetadataValue<tonic::metadata::Ascii>>);

impl tonic::service::Interceptor for Auth {
    fn call(&mut self, mut req: tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status> {
        if let Some(v) = &self.0 {
            req.metadata_mut().insert("authorization", v.clone());
        }
        Ok(req)
    }
}

type Ch = InterceptedService<Channel, Auth>;

pub use flux_proto::v1::ChangesetInfo;

/// Bytes per upload stream before starting a new one.
const UPLOAD_BATCH_BYTES: usize = 32 * 1024 * 1024;
/// Concurrent upload streams.
const UPLOAD_STREAMS: usize = 8;
/// Max ids per batch request.
const ID_BATCH: usize = 2_000;

#[derive(Clone)]
pub struct Client {
    repo: String,
    objects: ObjectServiceClient<Ch>,
    repos: RepoServiceClient<Ch>,
    writes: WriteServiceClient<Ch>,
    admin: AdminServiceClient<Ch>,
    reviews: ReviewServiceClient<Ch>,
    events: EventServiceClient<Ch>,
    auth: AuthServiceClient<Ch>,
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
        Self::connect_with_token(url, repo, None).await
    }

    /// Connect, authenticating with `token` (or `$FLUX_TOKEN` when `None`).
    pub async fn connect_with_token(url: &str, repo: &str, token: Option<&str>) -> Result<Self> {
        let channel = Endpoint::from_shared(url.to_string())?
            .tcp_nodelay(true)
            .http2_adaptive_window(true)
            .connect_timeout(std::time::Duration::from_secs(10))
            .connect()
            .await
            .with_context(|| format!("connecting to {url}"))?;
        let token = token.map(str::to_string).or_else(|| std::env::var("FLUX_TOKEN").ok()).filter(|t| !t.is_empty());
        Self::from_channel_with_token(channel, repo, token.as_deref())
    }

    pub fn from_channel(channel: Channel, repo: &str) -> Self {
        Self::from_channel_with_token(channel, repo, None).expect("no token")
    }

    pub fn from_channel_with_token(channel: Channel, repo: &str, token: Option<&str>) -> Result<Self> {
        let auth = Auth(match token {
            Some(t) => Some(format!("Bearer {}", t.trim()).parse().map_err(|_| anyhow!("token contains invalid characters"))?),
            None => None,
        });
        let ch = |c: &Channel| InterceptedService::new(c.clone(), auth.clone());
        Ok(Self {
            repo: repo.to_string(),
            objects: ObjectServiceClient::new(ch(&channel))
                .max_decoding_message_size(MAX_MESSAGE_BYTES)
                .max_encoding_message_size(MAX_MESSAGE_BYTES),
            repos: RepoServiceClient::new(ch(&channel))
                .max_decoding_message_size(MAX_MESSAGE_BYTES)
                .max_encoding_message_size(MAX_MESSAGE_BYTES),
            writes: WriteServiceClient::new(ch(&channel))
                .max_decoding_message_size(MAX_MESSAGE_BYTES)
                .max_encoding_message_size(MAX_MESSAGE_BYTES),
            admin: AdminServiceClient::new(ch(&channel)),
            reviews: ReviewServiceClient::new(ch(&channel)).max_decoding_message_size(MAX_MESSAGE_BYTES),
            events: EventServiceClient::new(ch(&channel)),
            auth: AuthServiceClient::new(ch(&channel)),
        })
    }

    /// Same connection, different repository.
    pub fn with_repo(&self, repo: &str) -> Self {
        Self { repo: repo.to_string(), ..self.clone() }
    }

    pub fn repo(&self) -> &str {
        &self.repo
    }

    // ───────────── admin ─────────────

    /// Exchange a password for an API token: `(token, user)`.
    pub async fn login(&self, login: &str, password: &str, label: &str) -> Result<(String, String)> {
        self.login_otp(login, password, "", label).await
    }

    /// Sign in with a password and, for accounts with two-factor auth, a
    /// code (an error containing `TWO_FACTOR_REQUIRED` asks for one).
    pub async fn login_otp(&self, login: &str, password: &str, otp: &str, label: &str) -> Result<(String, String)> {
        let r = self
            .auth
            .clone()
            .login(LoginRequest {
                login: login.into(),
                password: password.into(),
                label: label.into(),
                scopes: String::new(),
                expires_days: 0,
                otp: otp.into(),
            })
            .await
            .map_err(|s| anyhow!("{}", s.message()))?
            .into_inner();
        Ok((r.token, r.user))
    }

    pub async fn create_repo(&self, name: &str) -> Result<()> {
        self.create_repo_with(name, "").await
    }

    /// `visibility`: public, internal, private, or empty for the default.
    pub async fn create_repo_with(&self, name: &str, visibility: &str) -> Result<()> {
        self.admin
            .clone()
            .create_repo(CreateRepoRequest { name: name.into(), visibility: visibility.into() })
            .await
            .map_err(|s| anyhow!("{}", s.message()))?;
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
        self.log_range(rev, &[], 0, limit, first_parent, path).await
    }

    /// `hide..rev` (ancestors of `rev` not reachable from `hide`).
    pub async fn log_range(
        &self,
        rev: &str,
        hide: &[String],
        skip: u32,
        limit: u32,
        first_parent: bool,
        path: Option<&[u8]>,
    ) -> Result<Vec<CsInfo>> {
        let mut s = self
            .repos
            .clone()
            .log(LogRequest {
                repo: self.repo.clone(),
                rev: rev.into(),
                limit,
                first_parent,
                path: path.map(<[u8]>::to_vec).unwrap_or_default(),
                hide: hide.to_vec(),
                skip,
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
        let items: Vec<(ContentId, Bytes, Option<ContentId>)> =
            items.into_iter().zip(present).filter(|(item, present)| !present && seen.insert(item.0)).map(|(i, _)| i).collect();
        let results: Vec<Result<()>> = futures::stream::iter(delta_units(items))
            .map(|unit| async move {
                // Batches within a unit go in order, so every delta base is
                // stored before the objects that delta against it.
                for b in unit {
                    self.upload_batch(b).await?;
                }
                Ok(())
            })
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
        self.create_changesets_from_git(batch, &[]).await
    }

    /// Create changesets and record the git commits they came from (empty
    /// `git_shas`: none) in one call.
    pub async fn create_changesets_from_git(&self, batch: &[Changeset], git_shas: &[[u8; 20]]) -> Result<Vec<CsInfo>> {
        let infos = self
            .writes
            .clone()
            .create_changesets(CreateChangesetsRequest {
                repo: self.repo.clone(),
                changesets: batch.iter().map(flux_proto::changeset_to_pb).collect(),
                git_sha1s: git_shas.iter().map(|s| s.to_vec()).collect(),
                ids_only: true,
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
                // The server echoes ids only; the id still proves it stored
                // exactly this changeset.
                let id = ChangesetId::from_slice(&i.id)?;
                if id != cs.id() {
                    bail!("server computed changeset id {id} but client expected {}", cs.id());
                }
                Ok(CsInfo {
                    id,
                    generation: i.generation,
                    root_tree: TreeId::from_slice(&i.root_tree)?,
                    changeset: cs.clone(),
                    git_sha1: None,
                })
            })
            .collect()
    }

    /// Open a pipelined create stream: batches are sent without waiting
    /// for earlier ones to commit, and may use changesets of earlier
    /// batches as parents. Results arrive in order on the receiver.
    pub async fn create_stream(&self) -> Result<(CreateSender, CreateReceiver)> {
        let (tx, rx) = tokio::sync::mpsc::channel::<CreateChangesetsRequest>(2);
        let (etx, erx) = tokio::sync::mpsc::unbounded_channel();
        let responses = self
            .writes
            .clone()
            .create_changesets_stream(tokio_stream::wrappers::ReceiverStream::new(rx))
            .await
            .map_err(|s| anyhow!("create changesets: {}", s.message()))?
            .into_inner();
        Ok((CreateSender { repo: self.repo.clone(), tx, expect: etx }, CreateReceiver { responses, expect: erx }))
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

    // ───────────── history, search, landing ─────────────

    fn status(s: tonic::Status) -> anyhow::Error {
        anyhow!("{}", s.message())
    }

    pub async fn blame(&self, rev: &str, path: &[u8]) -> Result<(Vec<(u32, u32, ChangesetId)>, Vec<CsInfo>)> {
        let r = self
            .repos
            .clone()
            .blame(BlameRequest { repo: self.repo.clone(), rev: rev.into(), path: path.to_vec() })
            .await
            .map_err(Self::status)?
            .into_inner();
        let ranges = r
            .ranges
            .iter()
            .map(|x| Ok((x.start_line, x.line_count, ChangesetId::from_slice(&x.changeset_id)?)))
            .collect::<Result<Vec<_>>>()?;
        let infos = r.changesets.into_iter().map(CsInfo::try_from).collect::<Result<Vec<_>>>()?;
        Ok((ranges, infos))
    }

    pub async fn ancestry(&self, a: &str, b: &str) -> Result<AncestryResponse> {
        Ok(self
            .repos
            .clone()
            .ancestry(AncestryRequest { repo: self.repo.clone(), a: a.into(), b: b.into() })
            .await
            .map_err(Self::status)?
            .into_inner())
    }

    pub async fn search(&self, req: SearchRequest) -> Result<Vec<SearchMatch>> {
        let mut s =
            self.repos.clone().search(SearchRequest { repo: self.repo.clone(), ..req }).await.map_err(Self::status)?.into_inner();
        let mut out = Vec::new();
        while let Some(m) = s.next().await {
            out.push(m?);
        }
        Ok(out)
    }

    pub async fn find_files(&self, rev: &str, query: &str, limit: u32) -> Result<Vec<Vec<u8>>> {
        Ok(self
            .repos
            .clone()
            .find_files(FindFilesRequest { repo: self.repo.clone(), rev: rev.into(), query: query.into(), limit })
            .await
            .map_err(Self::status)?
            .into_inner()
            .paths)
    }

    pub async fn land(&self, rev: &str, target: &str, reason: &str) -> Result<LandResponse> {
        Ok(self
            .writes
            .clone()
            .land(LandRequest { repo: self.repo.clone(), rev: rev.into(), target_ref: target.into(), reason: reason.into() })
            .await
            .map_err(Self::status)?
            .into_inner())
    }

    pub async fn create_review(&self, title: &str, body: &str, head: &str, target: &str) -> Result<v1::Review> {
        Ok(self
            .reviews
            .clone()
            .create_review(CreateReviewRequest {
                repo: self.repo.clone(),
                title: title.into(),
                body: body.into(),
                head_rev: head.into(),
                target: target.into(),
            })
            .await
            .map_err(Self::status)?
            .into_inner())
    }

    pub async fn list_reviews(&self, state: &str) -> Result<Vec<v1::Review>> {
        Ok(self
            .reviews
            .clone()
            .list_reviews(ListReviewsRequest { repo: self.repo.clone(), state: state.into(), limit: 0 })
            .await
            .map_err(Self::status)?
            .into_inner()
            .reviews)
    }

    pub async fn get_review(&self, number: i64) -> Result<ReviewDetail> {
        Ok(self
            .reviews
            .clone()
            .get_review(GetReviewRequest { repo: self.repo.clone(), number })
            .await
            .map_err(Self::status)?
            .into_inner())
    }

    pub async fn update_review(&self, number: i64, action: &str, body: &str, head: &str) -> Result<v1::Review> {
        Ok(self
            .reviews
            .clone()
            .update_review(UpdateReviewRequest {
                repo: self.repo.clone(),
                number,
                action: action.into(),
                body: body.into(),
                head_rev: head.into(),
            })
            .await
            .map_err(Self::status)?
            .into_inner())
    }

    pub async fn land_review(&self, number: i64) -> Result<LandResponse> {
        Ok(self
            .reviews
            .clone()
            .land_review(LandReviewRequest { repo: self.repo.clone(), number })
            .await
            .map_err(Self::status)?
            .into_inner())
    }

    /// Stream repository events (empty repo name = all repositories).
    pub async fn watch(&self, all: bool) -> Result<tonic::Streaming<RepoEvent>> {
        Ok(self
            .events
            .clone()
            .watch(WatchRequest { repo: if all { String::new() } else { self.repo.clone() } })
            .await
            .map_err(Self::status)?
            .into_inner())
    }

    pub async fn whoami(&self) -> Result<WhoAmIResponse> {
        Ok(self.auth.clone().who_am_i(WhoAmIRequest {}).await.map_err(Self::status)?.into_inner())
    }
}

/// Group uploads so delta chains stay together: items linked by in-set
/// delta bases form one unit, and units are packed into batches of about
/// `UPLOAD_BATCH_BYTES`. A unit larger than that is split into consecutive
/// batches (uploaded in order); otherwise a version uploaded concurrently
/// with its base would be stored without a delta.
fn delta_units(items: Vec<(ContentId, Bytes, Option<ContentId>)>) -> Vec<Vec<Vec<(ContentId, Bytes, Option<ContentId>)>>> {
    let index: std::collections::HashMap<ContentId, usize> = items.iter().enumerate().map(|(i, it)| (it.0, i)).collect();
    // Union-find over in-set base links.
    let mut parent: Vec<usize> = (0..items.len()).collect();
    fn find(p: &mut [usize], mut i: usize) -> usize {
        while p[i] != i {
            p[i] = p[p[i]];
            i = p[i];
        }
        i
    }
    for (i, it) in items.iter().enumerate() {
        if let Some(j) = it.2.and_then(|b| index.get(&b).copied()) {
            let (a, b) = (find(&mut parent, i), find(&mut parent, j));
            parent[a] = b;
        }
    }
    // Groups keep input order, which already puts bases before versions.
    let mut groups: std::collections::BTreeMap<usize, Vec<(ContentId, Bytes, Option<ContentId>)>> = Default::default();
    let mut first_of: std::collections::HashMap<usize, usize> = Default::default();
    for (i, it) in items.into_iter().enumerate() {
        let root = find(&mut parent, i);
        let key = *first_of.entry(root).or_insert(i);
        groups.entry(key).or_default().push(it);
    }
    let mut units: Vec<Vec<Vec<(ContentId, Bytes, Option<ContentId>)>>> = Vec::new();
    let mut open: Vec<(ContentId, Bytes, Option<ContentId>)> = Vec::new();
    let mut open_bytes = 0;
    for (_, g) in groups {
        let bytes: usize = g.iter().map(|i| i.1.len()).sum();
        if bytes >= UPLOAD_BATCH_BYTES {
            let mut unit = vec![vec![]];
            let mut b = 0;
            for it in g {
                b += it.1.len();
                unit.last_mut().unwrap().push(it);
                if b >= UPLOAD_BATCH_BYTES {
                    unit.push(vec![]);
                    b = 0;
                }
            }
            unit.retain(|b| !b.is_empty());
            units.push(unit);
            continue;
        }
        if open_bytes + bytes > UPLOAD_BATCH_BYTES && !open.is_empty() {
            units.push(vec![std::mem::take(&mut open)]);
            open_bytes = 0;
        }
        open_bytes += bytes;
        open.extend(g);
    }
    if !open.is_empty() {
        units.push(vec![open]);
    }
    units
}

/// The sending half of [`Client::create_stream`].
pub struct CreateSender {
    repo: String,
    tx: tokio::sync::mpsc::Sender<CreateChangesetsRequest>,
    expect: tokio::sync::mpsc::UnboundedSender<Vec<ChangesetId>>,
}

impl CreateSender {
    /// Queue a batch (with the git commit of each changeset, or none).
    pub async fn send(&self, batch: &[Changeset], git_shas: &[[u8; 20]]) -> Result<()> {
        let _ = self.expect.send(batch.iter().map(Changeset::id).collect());
        self.tx
            .send(CreateChangesetsRequest {
                repo: self.repo.clone(),
                changesets: batch.iter().map(flux_proto::changeset_to_pb).collect(),
                git_sha1s: git_shas.iter().map(|s| s.to_vec()).collect(),
                ids_only: true,
            })
            .await
            .map_err(|_| anyhow!("create stream closed"))
    }
}

/// The receiving half of [`Client::create_stream`].
pub struct CreateReceiver {
    responses: tonic::Streaming<CreateChangesetsResponse>,
    expect: tokio::sync::mpsc::UnboundedReceiver<Vec<ChangesetId>>,
}

impl CreateReceiver {
    /// The ids of the next committed batch, checked against what was sent;
    /// `None` once the sender is dropped and every batch is answered.
    pub async fn next(&mut self) -> Result<Option<Vec<ChangesetId>>> {
        let Some(r) = self.responses.message().await.map_err(|s| anyhow!("create changesets: {}", s.message()))? else {
            return match self.expect.try_recv() {
                Ok(_) => Err(anyhow!("create stream ended early")),
                Err(_) => Ok(None),
            };
        };
        let want = self.expect.recv().await.ok_or_else(|| anyhow!("unexpected create response"))?;
        if r.changesets.len() != want.len() {
            bail!("server returned {} results for {} changesets", r.changesets.len(), want.len());
        }
        for (info, id) in r.changesets.iter().zip(&want) {
            if ChangesetId::from_slice(&info.id)? != *id {
                bail!("server computed changeset id {} but client expected {id}", ChangesetId::from_slice(&info.id)?);
            }
        }
        Ok(Some(want))
    }
}

#[cfg(test)]
mod delta_unit_tests {
    use super::*;

    #[test]
    fn chains_stay_together_and_in_order() {
        let id = |n: u8| ContentId::hash(&[n]);
        let big = Bytes::from(vec![0u8; UPLOAD_BATCH_BYTES / 3 + 1]);
        let items = vec![
            (id(1), big.clone(), None),
            (id(2), Bytes::from_static(b"x"), None),
            (id(3), big.clone(), Some(id(1))),
            (id(4), big.clone(), Some(id(3))),
            (id(5), Bytes::from_static(b"y"), Some(id(2))),
        ];
        let units = delta_units(items);
        let pos = |want: ContentId| {
            units.iter().enumerate().find_map(|(u, unit)| {
                unit.iter().enumerate().find_map(|(b, batch)| batch.iter().position(|i| i.0 == want).map(|p| (u, b, p)))
            })
        };
        let (u1, b1, p1) = pos(id(1)).unwrap();
        let (u3, b3, p3) = pos(id(3)).unwrap();
        let (u4, _, _) = pos(id(4)).unwrap();
        assert_eq!((u1, u3, u4), (u1, u1, u1), "one chain, one unit");
        assert!((b1, p1) < (b3, p3), "base first");
        assert_eq!(pos(id(2)).unwrap().0, pos(id(5)).unwrap().0);
    }
}
