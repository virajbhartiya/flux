//! flux authoritative server (SPEC §9.1).

pub mod core;
pub mod grpc;

use std::net::SocketAddr;
use std::sync::Arc;

use flux_proto::MAX_MESSAGE_BYTES;
use flux_proto::v1::admin_service_server::AdminServiceServer;
use flux_proto::v1::object_service_server::ObjectServiceServer;
use flux_proto::v1::repo_service_server::RepoServiceServer;
use flux_proto::v1::write_service_server::WriteServiceServer;
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;

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
            blob_url: url::Url::from_directory_path(dir.join("blobs"))
                .map_err(|_| anyhow::anyhow!("bad data dir"))?
                .to_string(),
        })
    }
}

pub async fn open(cfg: &StorageConfig) -> anyhow::Result<Arc<Flux>> {
    let meta = flux_store::open_meta(&cfg.meta_url).await?;
    let blobs = flux_store::BlobStore::open(&cfg.blob_url)?;
    Ok(Arc::new(Flux::new(meta, blobs)))
}

/// Serve all services on an already-bound listener until `shutdown` resolves.
pub async fn serve(
    flux: Arc<Flux>,
    listener: TcpListener,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    let svc = grpc::Services::new(flux);
    tonic::transport::Server::builder()
        .tcp_nodelay(true)
        .add_service(
            ObjectServiceServer::new(svc.clone())
                .max_decoding_message_size(MAX_MESSAGE_BYTES)
                .max_encoding_message_size(MAX_MESSAGE_BYTES),
        )
        .add_service(
            RepoServiceServer::new(svc.clone())
                .max_decoding_message_size(MAX_MESSAGE_BYTES)
                .max_encoding_message_size(MAX_MESSAGE_BYTES),
        )
        .add_service(
            WriteServiceServer::new(svc.clone())
                .max_decoding_message_size(MAX_MESSAGE_BYTES)
                .max_encoding_message_size(MAX_MESSAGE_BYTES),
        )
        .add_service(AdminServiceServer::new(svc))
        .serve_with_incoming_shutdown(TcpListenerStream::new(listener), shutdown)
        .await?;
    Ok(())
}

/// Bind and serve on `addr` (helper for tests and the binary).
pub async fn bind(addr: SocketAddr) -> anyhow::Result<(TcpListener, SocketAddr)> {
    let l = TcpListener::bind(addr).await?;
    let local = l.local_addr()?;
    Ok((l, local))
}
