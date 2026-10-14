//! flux authoritative server (SPEC §9.1).

pub mod auth;
pub mod cache;
pub mod core;
pub mod graph;
pub mod grpc;
pub mod history;
pub mod land;
pub mod metrics;
pub mod ops;
pub mod repo_config;
pub mod search;
pub mod web;

use std::net::SocketAddr;
use std::sync::Arc;

use flux_proto::MAX_MESSAGE_BYTES;
use flux_proto::v1::admin_service_server::AdminServiceServer;
use flux_proto::v1::auth_service_server::AuthServiceServer;
use flux_proto::v1::event_service_server::EventServiceServer;
use flux_proto::v1::object_service_server::ObjectServiceServer;
use flux_proto::v1::repo_service_server::RepoServiceServer;
use flux_proto::v1::review_service_server::ReviewServiceServer;
use flux_proto::v1::write_service_server::WriteServiceServer;
use tokio::net::TcpListener;

pub use crate::core::{Flux, FluxError};

/// Storage configuration.
#[derive(Debug, Clone)]
pub struct StorageConfig {
    pub meta_url: String,
    pub blob_url: String,
}

impl StorageConfig {
    /// SQLite + local files under one directory: the all-in-one layout.
    pub fn data_dir(dir: &std::path::Path) -> anyhow::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let dir = dir.canonicalize()?;
        Ok(Self {
            meta_url: format!("sqlite://{}", dir.join("meta.db").display()),
            blob_url: url::Url::from_directory_path(dir.join("blobs")).map_err(|_| anyhow::anyhow!("bad data dir"))?.to_string(),
        })
    }
}

pub async fn open(cfg: &StorageConfig) -> anyhow::Result<Arc<Flux>> {
    open_with(cfg, auth::AuthMode::Open).await
}

pub async fn open_with(cfg: &StorageConfig, auth: auth::AuthMode) -> anyhow::Result<Arc<Flux>> {
    let meta = flux_store::open_meta(&cfg.meta_url).await?;
    let blobs = flux_store::BlobStore::open(&cfg.blob_url)?;
    Ok(Arc::new(Flux::new(meta, blobs).with_auth_mode(auth)))
}

/// Warm caches in the background after startup: load commit graphs and
/// derive history indexes for every ref head, so the first page views and
/// `log -- path` queries are fast.
pub fn warm(flux: &Arc<Flux>) {
    let flux = flux.clone();
    tokio::spawn(async move {
        let started = std::time::Instant::now();
        let Ok(repos) = flux.meta.list_repos().await else { return };
        for r in repos {
            if let Err(e) = flux.repo_graph(r.id).await {
                tracing::warn!(repo = %r.name, error = %e, "warm: graph load failed");
                continue;
            }
            let Ok(refs) = flux.meta.list_refs(r.id, b"", 10_000).await else { continue };
            for (_, v) in refs {
                if let Err(e) = flux.lastmod(r.id, &v.target).await {
                    tracing::warn!(repo = %r.name, error = %e, "warm: lastmod derivation failed");
                }
            }
        }
        tracing::info!(elapsed_ms = started.elapsed().as_millis() as u64, "caches warm");
    });
}

/// What to serve on the listener.
#[derive(Debug, Clone, Copy)]
pub struct ServeOptions {
    /// Serve the web UI alongside gRPC (same port, HTTP/1.1 and HTTP/2).
    pub web: bool,
}

impl Default for ServeOptions {
    fn default() -> Self {
        Self { web: true }
    }
}

macro_rules! grpc_service {
    ($router:expr, $server:ty, $svc:expr) => {{
        let s = <$server>::new($svc).max_decoding_message_size(MAX_MESSAGE_BYTES).max_encoding_message_size(MAX_MESSAGE_BYTES);
        $router.route_service(
            &format!("/{}/{{*rest}}", <$server as tonic::server::NamedService>::NAME),
            tower::ServiceExt::map_request(s, |req: axum::http::Request<axum::body::Body>| req.map(tonic::body::Body::new)),
        )
    }};
}

/// The combined application: every gRPC service plus (optionally) the web UI.
pub fn app(flux: Arc<Flux>, opts: ServeOptions) -> axum::Router {
    use tower_http::compression::Predicate as _;
    let svc = grpc::Services::new(flux.clone());
    let router = if opts.web {
        web::router(flux)
    } else {
        axum::Router::new().fallback(|| async { (axum::http::StatusCode::NOT_FOUND, "not found") })
    };
    let router = grpc_service!(router, ObjectServiceServer<grpc::Services>, svc.clone());
    let router = grpc_service!(router, RepoServiceServer<grpc::Services>, svc.clone());
    let router = grpc_service!(router, WriteServiceServer<grpc::Services>, svc.clone());
    let router = grpc_service!(router, ReviewServiceServer<grpc::Services>, svc.clone());
    let router = grpc_service!(router, EventServiceServer<grpc::Services>, svc.clone());
    let router = grpc_service!(router, AuthServiceServer<grpc::Services>, svc.clone());
    let router = grpc_service!(router, AdminServiceServer<grpc::Services>, svc);
    router.layer(axum::middleware::from_fn(observe)).layer(
        tower_http::compression::CompressionLayer::new().quality(tower_http::CompressionLevel::Fastest).compress_when(
            tower_http::compression::predicate::SizeAbove::new(2048)
                .and(tower_http::compression::predicate::NotForContentType::GRPC)
                .and(tower_http::compression::predicate::NotForContentType::IMAGES)
                .and(tower_http::compression::predicate::NotForContentType::SSE),
        ),
    )
}

/// Request metrics for both surfaces.
async fn observe(req: axum::extract::Request, next: axum::middleware::Next) -> axum::response::Response {
    let started = std::time::Instant::now();
    let grpc = req.headers().get(axum::http::header::CONTENT_TYPE).is_some_and(|v| v.as_bytes().starts_with(b"application/grpc"));
    let resp = next.run(req).await;
    let error = resp.status().is_server_error();
    metrics::observe(grpc, started, error);
    resp
}

/// Serve all services on an already-bound listener until `shutdown` resolves.
pub async fn serve(
    flux: Arc<Flux>,
    listener: TcpListener,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    serve_with(flux, listener, ServeOptions::default(), shutdown).await
}

pub async fn serve_with(
    flux: Arc<Flux>,
    listener: TcpListener,
    opts: ServeOptions,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    use axum::serve::ListenerExt;
    let listener = listener.tap_io(|tcp| {
        let _ = tcp.set_nodelay(true);
    });
    // Long-lived streams (events, watches) would hold a graceful shutdown
    // open forever: drain for at most 10 s after the signal.
    let (tx, rx) = tokio::sync::watch::channel(false);
    let signal = async move {
        shutdown.await;
        let _ = tx.send(true);
    };
    let mut rx2 = rx.clone();
    let server = axum::serve(listener, app(flux, opts)).with_graceful_shutdown(signal);
    tokio::select! {
        r = server => r?,
        _ = async move {
            let _ = rx2.wait_for(|v| *v).await;
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        } => tracing::warn!("shutdown: closing connections still open after 10 s"),
    }
    drop(rx);
    Ok(())
}

/// Bind and serve on `addr` (helper for tests and the binary).
pub async fn bind(addr: SocketAddr) -> anyhow::Result<(TcpListener, SocketAddr)> {
    let l = TcpListener::bind(addr).await?;
    let local = l.local_addr()?;
    Ok((l, local))
}
