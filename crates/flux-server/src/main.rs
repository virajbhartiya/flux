use std::path::PathBuf;

use clap::Parser;
use flux_server::StorageConfig;

/// flux authoritative server.
///
/// All-in-one mode: `flux-server --data-dir ./data` (SQLite + local blobs).
/// Production: `--meta-url postgres://... --blob-url s3://bucket/prefix`.
#[derive(Parser)]
#[command(version)]
struct Args {
    #[arg(long, env = "FLUX_LISTEN", default_value = "127.0.0.1:7878")]
    listen: std::net::SocketAddr,
    /// Directory for the all-in-one SQLite + filesystem layout.
    #[arg(long, env = "FLUX_DATA_DIR", conflicts_with_all = ["meta_url", "blob_url"])]
    data_dir: Option<PathBuf>,
    #[arg(long, env = "FLUX_META_URL", requires = "blob_url")]
    meta_url: Option<String>,
    #[arg(long, env = "FLUX_BLOB_URL", requires = "meta_url")]
    blob_url: Option<String>,
    /// Create these repositories at startup if they don't exist.
    #[arg(long = "create-repo")]
    create_repos: Vec<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let args = Args::parse();
    let cfg = match (args.data_dir, args.meta_url, args.blob_url) {
        (Some(dir), _, _) => StorageConfig::data_dir(&dir)?,
        (None, Some(meta_url), Some(blob_url)) => StorageConfig { meta_url, blob_url },
        _ => StorageConfig::data_dir(&PathBuf::from(".flux-data"))?,
    };
    let flux = flux_server::open(&cfg).await?;
    for name in &args.create_repos {
        if flux.meta.get_repo(name).await?.is_none() {
            flux.create_repo(name).await?;
            tracing::info!(repo = %name, "created repository");
        }
    }
    let (listener, addr) = flux_server::bind(args.listen).await?;
    tracing::info!(%addr, meta = %cfg.meta_url, blobs = %cfg.blob_url, "flux-server listening");
    flux_server::serve(flux, listener, async {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("shutting down");
    })
    .await
}
