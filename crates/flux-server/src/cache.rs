//! `flux-cache`: a regional cache tier (SPEC §9.2).
//!
//! Serves `ObjectService` (immutable, content-addressed trees and contents)
//! from a local segment store, fetching misses from the upstream server and
//! verifying every object against its id before storing or serving it. Every
//! other request (RepoService, writes, reviews, events, the web UI) is
//! reverse-proxied to the upstream unchanged, so clients can point at a
//! cache exactly as they would at the server.
//!
//! Authorization: cached objects are only served to callers the upstream
//! accepts as readers; the decision is cached per token for 60 s.

use std::collections::{HashMap, VecDeque};
use std::pin::Pin;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use flux_client::Client;
use flux_core::{ContentId, Tree, TreeId};
use flux_proto::v1::object_service_server::{ObjectService, ObjectServiceServer};
use flux_proto::v1::*;
use flux_proto::{FRAME_BYTES, MAX_MESSAGE_BYTES, content_id, tree_id};
use flux_store::{BlobStore, PutObject};
use futures::Stream;
use hyper_util::client::legacy::Client as HttpClient;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use tonic::{Status, transport::Endpoint};

type RpcStream<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send>>;

pub struct Cache {
    upstream: String,
    channel: tonic::transport::Channel,
    pub blobs: BlobStore,
    readers: moka::future::Cache<String, bool>,
    h1: HttpClient<HttpConnector, Body>,
    h2: HttpClient<HttpConnector, Body>,
    hits: std::sync::atomic::AtomicU64,
    misses: std::sync::atomic::AtomicU64,
}

impl Cache {
    pub async fn new(upstream: &str, blob_url: &str) -> anyhow::Result<Arc<Self>> {
        let upstream = upstream.trim_end_matches('/').to_string();
        let channel = Endpoint::from_shared(upstream.clone())?.tcp_nodelay(true).http2_adaptive_window(true).connect_lazy();
        let mut conn = HttpConnector::new();
        conn.set_nodelay(true);
        Ok(Arc::new(Self {
            upstream,
            channel,
            blobs: BlobStore::open(blob_url)?,
            readers: moka::future::Cache::builder()
                .time_to_live(std::time::Duration::from_secs(60))
                .max_capacity(100_000)
                .build(),
            h1: HttpClient::builder(TokioExecutor::new()).build(conn.clone()),
            h2: HttpClient::builder(TokioExecutor::new()).http2_only(true).build(conn),
            hits: Default::default(),
            misses: Default::default(),
        }))
    }

    fn token<T>(req: &tonic::Request<T>) -> Option<String> {
        req.metadata()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(|v| v.strip_prefix("Bearer ").unwrap_or(v).to_string())
    }

    fn client(&self, repo: &str, token: Option<&str>) -> Result<Client, Status> {
        Client::from_channel_with_token(self.channel.clone(), repo, token).map_err(|e| Status::invalid_argument(e.to_string()))
    }

    /// May this caller read? Asks the upstream once per token per minute.
    async fn check<T>(&self, req: &tonic::Request<T>) -> Result<Option<String>, Status> {
        let token = Self::token(req);
        let key = token.clone().unwrap_or_default();
        if let Some(ok) = self.readers.get(&key).await {
            return if ok { Ok(token) } else { Err(Status::unauthenticated("this server requires a token")) };
        }
        let who = self.client("", token.as_deref())?.whoami().await;
        let ok = match &who {
            Ok(w) => !(w.auth_mode == "private" && w.user.is_empty()),
            Err(_) => false,
        };
        self.readers.insert(key, ok).await;
        match who {
            Ok(_) if ok => Ok(token),
            Ok(_) => Err(Status::unauthenticated("this server requires a token")),
            Err(e) => Err(Status::unauthenticated(e.to_string())),
        }
    }

    fn count(&self, hit: bool, n: u64) {
        let c = if hit { &self.hits } else { &self.misses };
        c.fetch_add(n, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn stats(&self) -> (u64, u64) {
        (self.hits.load(std::sync::atomic::Ordering::Relaxed), self.misses.load(std::sync::atomic::Ordering::Relaxed))
    }
}

#[derive(Clone)]
pub struct CacheObjects(pub Arc<Cache>);

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

/// Walk the request entirely from the local cache; `None` on any miss.
async fn local_trees(blobs: &BlobStore, req: &GetTreesRequest) -> Result<Option<Vec<TreeObject>>, Status> {
    let mut out = Vec::new();
    let mut queue: VecDeque<(TreeId, Vec<u8>, u32)> = VecDeque::new();
    for (i, raw) in req.tree_ids.iter().enumerate() {
        queue.push_back((tree_id(raw)?, if i == 0 { req.path_hint.clone() } else { vec![] }, 0));
    }
    let depth = req.depth.min(64);
    while let Some((id, path, level)) = queue.pop_front() {
        let Some(tree) = blobs.get_tree(&id).await.map_err(|e| Status::internal(e.to_string()))? else {
            return Ok(None);
        };
        if level < depth {
            for e in tree.entries() {
                if let Some(sub) = e.tree_id() {
                    let mut p = path.clone();
                    if !p.is_empty() {
                        p.push(b'/');
                    }
                    p.extend_from_slice(e.name.as_bytes());
                    if sparse_allows(&p, &req.sparse_prefixes) {
                        queue.push_back((sub, p, level + 1));
                    }
                }
            }
        }
        out.push(TreeObject { id: id.as_bytes().to_vec(), path, canonical: tree.encode().into() });
    }
    Ok(Some(out))
}

#[tonic::async_trait]
impl ObjectService for CacheObjects {
    type GetTreesStream = RpcStream<TreeObject>;
    type GetContentsStream = RpcStream<ContentFrame>;
    type GetContentMetaStream = RpcStream<ContentMetaResult>;

    async fn get_trees(&self, req: tonic::Request<GetTreesRequest>) -> Result<tonic::Response<Self::GetTreesStream>, Status> {
        let token = self.0.check(&req).await?;
        let req = req.into_inner();
        let objs = match local_trees(&self.0.blobs, &req).await? {
            Some(objs) => {
                self.0.count(true, objs.len() as u64);
                objs
            }
            None => {
                // One upstream round trip for the whole (sparse) expansion.
                let ids: Vec<TreeId> = req.tree_ids.iter().map(|b| tree_id(b)).collect::<Result<_, _>>()?;
                let client = self.0.client(&req.repo, token.as_deref())?;
                let got = client
                    .get_trees(&ids, req.depth, &req.sparse_prefixes, &req.path_hint)
                    .await
                    .map_err(|e| Status::unavailable(format!("upstream: {e}")))?;
                self.0.count(false, got.len() as u64);
                let trees: Vec<(Arc<Tree>, Option<TreeId>)> = got.iter().map(|(_, _, t)| (Arc::new(t.clone()), None)).collect();
                self.0.blobs.put_trees(trees).await.map_err(|e| Status::internal(e.to_string()))?;
                got.into_iter()
                    .map(|(id, path, t)| TreeObject { id: id.as_bytes().to_vec(), path, canonical: t.encode().into() })
                    .collect()
            }
        };
        Ok(tonic::Response::new(Box::pin(futures::stream::iter(objs.into_iter().map(Ok)))))
    }

    async fn get_contents(
        &self,
        req: tonic::Request<GetContentsRequest>,
    ) -> Result<tonic::Response<Self::GetContentsStream>, Status> {
        let token = self.0.check(&req).await?;
        let req = req.into_inner();
        let ids: Vec<ContentId> = req.content_ids.iter().map(|b| content_id(b)).collect::<Result<_, _>>()?;
        let mut have: HashMap<ContentId, Bytes> = HashMap::new();
        let mut missing = Vec::new();
        for id in &ids {
            match self.0.blobs.get_content_bytes(id).await {
                Ok(Some(b)) => {
                    have.insert(*id, b);
                }
                _ => missing.push(*id),
            }
        }
        self.0.count(true, have.len() as u64);
        if !missing.is_empty() {
            self.0.count(false, missing.len() as u64);
            let client = self.0.client(&req.repo, token.as_deref())?;
            let mut fetched: Vec<(Bytes, Option<ContentId>)> = Vec::new();
            client
                .get_contents(&missing, |id, data| {
                    // The client verified the hash already.
                    let b = Bytes::from(data);
                    have.insert(id, b.clone());
                    fetched.push((b, None));
                    Ok(())
                })
                .await
                .map_err(|e| Status::unavailable(format!("upstream: {e}")))?;
            let small: Vec<PutObject> = fetched
                .iter()
                .filter(|(d, _)| d.len() <= flux_store::CHUNK_THRESHOLD)
                .map(|(d, _)| PutObject {
                    kind: flux_core::ObjectKind::Content,
                    id: *ContentId::hash(d).as_bytes(),
                    data: d.clone(),
                    base: None,
                })
                .collect();
            self.0.blobs.put_objects(small).await.map_err(|e| Status::internal(e.to_string()))?;
            let large: Vec<(Bytes, Option<ContentId>)> =
                fetched.into_iter().filter(|(d, _)| d.len() > flux_store::CHUNK_THRESHOLD).collect();
            if !large.is_empty() {
                self.0.blobs.put_contents(large).await.map_err(|e| Status::internal(e.to_string()))?;
            }
        }
        let mut frames = Vec::new();
        for id in ids {
            let Some(data) = have.get(&id) else {
                return Err(Status::not_found(format!("content {id} not found")));
            };
            let mut off = 0usize;
            loop {
                let end = (off + FRAME_BYTES).min(data.len());
                frames.push(Ok(ContentFrame {
                    content_id: id.as_bytes().to_vec(),
                    offset: off as u64,
                    data: data.slice(off..end),
                    last: end == data.len(),
                }));
                if end == data.len() {
                    break;
                }
                off = end;
            }
        }
        Ok(tonic::Response::new(Box::pin(futures::stream::iter(frames))))
    }

    async fn get_content_meta(
        &self,
        req: tonic::Request<GetContentMetaRequest>,
    ) -> Result<tonic::Response<Self::GetContentMetaStream>, Status> {
        let token = Cache::token(&req);
        let req = req.into_inner();
        let mut client = flux_proto::v1::object_service_client::ObjectServiceClient::new(self.0.channel.clone());
        let mut r = tonic::Request::new(req);
        if let Some(t) = token {
            r.metadata_mut()
                .insert("authorization", format!("Bearer {t}").parse().map_err(|_| Status::invalid_argument("bad token"))?);
        }
        let s = client.get_content_meta(r).await?.into_inner();
        Ok(tonic::Response::new(Box::pin(s)))
    }
}

/// Reverse-proxy anything that isn't served locally.
async fn proxy(State(cache): State<Arc<Cache>>, req: Request) -> Response {
    let grpc = req.headers().get(axum::http::header::CONTENT_TYPE).is_some_and(|v| v.as_bytes().starts_with(b"application/grpc"));
    let path_q = req.uri().path_and_query().map(|p| p.as_str().to_string()).unwrap_or_else(|| "/".into());
    let target = format!("{}{path_q}", cache.upstream);
    let (mut parts, body) = req.into_parts();
    parts.uri = match target.parse() {
        Ok(u) => u,
        Err(e) => return (axum::http::StatusCode::BAD_GATEWAY, e.to_string()).into_response(),
    };
    parts.headers.remove(axum::http::header::HOST);
    if !grpc {
        parts.version = axum::http::Version::HTTP_11;
    }
    let upstream_req = axum::http::Request::from_parts(parts, body);
    let client = if grpc { &cache.h2 } else { &cache.h1 };
    match client.request(upstream_req).await {
        Ok(resp) => resp.map(Body::new).into_response(),
        Err(e) => (axum::http::StatusCode::BAD_GATEWAY, format!("upstream unavailable: {e}")).into_response(),
    }
}

async fn cache_metrics(State(cache): State<Arc<Cache>>) -> String {
    let (h, m) = cache.stats();
    let b = cache.blobs.stats();
    format!(
        "# TYPE flux_cache_hits_total counter\nflux_cache_hits_total {h}\n# TYPE flux_cache_misses_total counter\nflux_cache_misses_total {m}\n\
         # TYPE flux_cache_objects gauge\nflux_cache_objects {}\n# TYPE flux_cache_bytes gauge\nflux_cache_bytes {}\n",
        b.objects, b.bytes
    )
}

pub fn app(cache: Arc<Cache>) -> axum::Router {
    let objects = ObjectServiceServer::new(CacheObjects(cache.clone()))
        .max_decoding_message_size(MAX_MESSAGE_BYTES)
        .max_encoding_message_size(MAX_MESSAGE_BYTES);
    let name = <ObjectServiceServer<CacheObjects> as tonic::server::NamedService>::NAME;
    axum::Router::new()
        .route_service(
            &format!("/{name}/{{*rest}}"),
            tower::ServiceExt::map_request(objects, |req: axum::http::Request<Body>| req.map(tonic::body::Body::new)),
        )
        .route("/-/cache/metrics", axum::routing::get(cache_metrics))
        .route("/-/cache/healthz", axum::routing::get(|| async { "ok" }))
        .fallback(proxy)
        .with_state(cache)
}

pub async fn serve(
    cache: Arc<Cache>,
    listener: tokio::net::TcpListener,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    use axum::serve::ListenerExt;
    let listener = listener.tap_io(|tcp| {
        let _ = tcp.set_nodelay(true);
    });
    axum::serve(listener, app(cache)).with_graceful_shutdown(shutdown).await?;
    Ok(())
}
