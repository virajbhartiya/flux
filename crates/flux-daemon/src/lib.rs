//! `fluxd`: the per-machine cache daemon (SPEC §9.3).
//!
//! A local instance of the cache tier for one upstream server: immutable
//! trees and file contents are served from local disk (verified on the way
//! in), and everything else is proxied upstream unchanged. On top of that
//! it watches the upstream for ref moves in the repositories it has served
//! and prefetches the new trees and small files, so `flux sync`, checkouts
//! and `flux cat` of fresh revisions are local reads.
//!
//! The daemon advertises itself in a state file (see
//! [`flux_client::daemon`]); the `flux` CLI routes through it whenever one
//! is running for the server it talks to.

use std::sync::Arc;
use std::time::Duration;

use flux_client::Client;
use flux_client::daemon::DaemonState;
use flux_server::cache::Cache;

/// Bytes of file contents prefetched per ref move.
const PREFETCH_CONTENT_BUDGET: u64 = 256 << 20;

pub struct Options {
    pub upstream: String,
    /// Cache directory; defaults to the per-upstream directory.
    pub dir: Option<std::path::PathBuf>,
    pub listen: std::net::SocketAddr,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

/// Run until `shutdown` resolves or the shutdown endpoint is called.
pub async fn run(opts: Options, shutdown: impl std::future::Future<Output = ()> + Send + 'static) -> anyhow::Result<()> {
    let upstream = opts.upstream.trim_end_matches('/').to_string();
    let dir = match opts.dir {
        Some(d) => d,
        None => flux_client::daemon::dir_for(&upstream).ok_or_else(|| anyhow::anyhow!("no cache directory"))?,
    };
    std::fs::create_dir_all(&dir)?;
    let blobs = dir.join("blobs");
    std::fs::create_dir_all(&blobs)?;
    let blob_url = flux_store::file_url(&blobs)?;
    let cache = Cache::new(&upstream, &blob_url).await?;

    let listener = tokio::net::TcpListener::bind(opts.listen).await?;
    let addr = listener.local_addr()?;
    let mut secret = [0u8; 16];
    getrandom::fill(&mut secret).map_err(|e| anyhow::anyhow!("random: {e}"))?;
    let state = DaemonState {
        pid: std::process::id(),
        url: format!("http://{addr}"),
        upstream: upstream.clone(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        started_ms: now_ms(),
        secret: hex::encode(secret),
    };
    let state_path = dir.join("fluxd.json");
    write_private(&state_path, &serde_json::to_vec_pretty(&state)?)?;
    tracing::info!(url = %state.url, upstream = %upstream, dir = %dir.display(), "fluxd listening");

    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let app = flux_server::cache::app(cache.clone()).merge(axum::Router::new().route(
        "/-/fluxd/shutdown",
        axum::routing::post({
            let (secret, stop_tx) = (state.secret.clone(), stop_tx.clone());
            move |headers: axum::http::HeaderMap| async move {
                if headers.get("x-fluxd-secret").and_then(|v| v.to_str().ok()) != Some(secret.as_str()) {
                    return axum::http::StatusCode::FORBIDDEN;
                }
                let _ = stop_tx.send(true);
                axum::http::StatusCode::OK
            }
        }),
    ));
    let prefetcher = tokio::spawn(prefetch_loop(cache.clone()));
    let mut stop = stop_rx.clone();
    let served = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            tokio::select! {
                _ = shutdown => {}
                _ = stop.wait_for(|s| *s) => {}
            }
        })
        .await;
    prefetcher.abort();
    // Only remove the state file if it is still ours.
    let current: Option<DaemonState> = std::fs::read(&state_path).ok().and_then(|b| serde_json::from_slice(&b).ok());
    if current.is_some_and(|s| s.pid == state.pid && s.url == state.url) {
        let _ = std::fs::remove_file(&state_path);
    }
    served?;
    Ok(())
}

fn write_private(path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    let tmp = path.with_extension("tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        f.write_all(data)?;
    }
    std::fs::rename(tmp, path)
}

/// Follow upstream ref moves and prefetch new revisions of repositories
/// that local clients have read. Reconnects with backoff.
async fn prefetch_loop(cache: Arc<Cache>) {
    let mut backoff = Duration::from_secs(1);
    loop {
        // Wait until some repository has been used (and we have its token).
        let Some((_, token)) = cache.repos().into_iter().next() else {
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        };
        match watch(&cache, token.as_deref()).await {
            Ok(()) => backoff = Duration::from_secs(1),
            Err(e) => {
                tracing::debug!(error = %e, "fluxd: event stream ended");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(60));
            }
        }
    }
}

/// Refs per repository brought up to date after (re)subscribing.
const CATCH_UP_REFS: usize = 64;

async fn watch(cache: &Arc<Cache>, token: Option<&str>) -> anyhow::Result<()> {
    let client = Client::connect_with_token(cache.upstream(), "", token).await?;
    let mut events = client.watch(true).await?;
    // Moves that happened before the subscription (or while disconnected)
    // produce no events: catch up on the current heads.
    for (repo, token) in cache.repos() {
        let c = Client::connect_with_token(cache.upstream(), &repo, token.as_deref()).await?;
        for r in c.list_refs(b"").await?.into_iter().take(CATCH_UP_REFS) {
            spawn_prefetch(cache.clone(), repo.clone(), token.clone(), r.target, r.name);
        }
    }
    while let Some(ev) = events.message().await? {
        if ev.kind != "ref" || ev.new.is_empty() {
            continue;
        }
        let Some((repo, token)) = cache.repos().into_iter().find(|(r, _)| *r == ev.repo) else { continue };
        spawn_prefetch(cache.clone(), repo, token, ev.new, ev.ref_name);
    }
    Ok(())
}

fn spawn_prefetch(cache: Arc<Cache>, repo: String, token: Option<String>, target: Vec<u8>, ref_name: Vec<u8>) {
    tokio::spawn(async move {
        let started = std::time::Instant::now();
        let result = async {
            let c = Client::connect_with_token(cache.upstream(), &repo, token.as_deref()).await?;
            let info = c.resolve(&hex::encode(&target)).await?;
            cache.prefetch(&repo, token.as_deref(), info.root_tree, PREFETCH_CONTENT_BUDGET).await
        }
        .await;
        match result {
            Ok((0, 0)) => {}
            Ok((trees, contents)) => tracing::info!(
                repo,
                trees,
                contents,
                ms = started.elapsed().as_millis() as u64,
                "fluxd: prefetched {}",
                String::from_utf8_lossy(&ref_name)
            ),
            Err(e) => tracing::warn!(repo, error = %e, "fluxd: prefetch failed"),
        }
    });
}
