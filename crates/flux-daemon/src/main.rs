//! `fluxd --upstream URL`: run the per-machine cache daemon. Usually started
//! by `flux daemon start` (or automatically, after `flux daemon enable`).

use clap::Parser;

#[derive(Parser)]
#[command(name = "fluxd", version, about = "flux per-machine cache daemon")]
struct Args {
    /// Upstream flux server URL.
    #[arg(long, env = "FLUX_SERVER")]
    upstream: String,
    /// Cache directory (default: per-upstream directory under the user cache dir).
    #[arg(long)]
    dir: Option<std::path::PathBuf>,
    /// Loopback address to listen on (port 0 picks a free port).
    #[arg(long, default_value = "127.0.0.1:0")]
    listen: std::net::SocketAddr,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let args = Args::parse();
    if !args.listen.ip().is_loopback() {
        anyhow::bail!("fluxd only listens on loopback addresses; run `flux-server cache` for a shared cache");
    }
    flux_daemon::run(flux_daemon::Options { upstream: args.upstream, dir: args.dir, listen: args.listen }, async {
        #[cfg(unix)]
        {
            let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("signal handler");
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
        }
        #[cfg(not(unix))]
        let _ = tokio::signal::ctrl_c().await;
    })
    .await
}
