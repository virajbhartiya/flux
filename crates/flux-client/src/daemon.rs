//! Discovering `fluxd`, the per-machine cache daemon (SPEC §9.3).
//!
//! One daemon runs per upstream server. It records where it listens in a
//! state file under the user's cache directory; clients that find a live
//! daemon for their server talk to it instead, and it serves immutable
//! objects from local disk and proxies everything else upstream.

use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonState {
    pub pid: u32,
    /// Where the daemon listens, e.g. `http://127.0.0.1:53123`.
    pub url: String,
    pub upstream: String,
    pub version: String,
    pub started_ms: i64,
    /// Required by the shutdown endpoint (the state file is private to the user).
    pub secret: String,
}

/// The daemon's directory for an upstream: blobs, logs and the state file.
pub fn dir_for(upstream: &str) -> Option<PathBuf> {
    let key = blake3::hash(upstream.trim_end_matches('/').as_bytes()).to_hex();
    Some(dirs::cache_dir()?.join("flux").join("fluxd").join(&key[..16]))
}

pub fn state_path(upstream: &str) -> Option<PathBuf> {
    Some(dir_for(upstream)?.join("fluxd.json"))
}

pub fn read_state(upstream: &str) -> Option<DaemonState> {
    serde_json::from_slice(&std::fs::read(state_path(upstream)?).ok()?).ok()
}

/// The URL of a live daemon for `upstream`, if one is running. Costs one
/// loopback connect (well under a millisecond).
pub fn live_url(upstream: &str) -> Option<String> {
    let st = read_state(upstream)?;
    let addr: std::net::SocketAddr = st.url.strip_prefix("http://")?.parse().ok()?;
    std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(50)).ok()?;
    Some(st.url)
}

/// User preference: start a daemon automatically when none is running.
pub fn auto_start_enabled() -> bool {
    config_path()
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .is_some_and(|v| v["daemon"] == true)
}

pub fn set_auto_start(on: bool) -> std::io::Result<()> {
    let p = config_path().ok_or_else(|| std::io::Error::other("no config directory"))?;
    let mut v: serde_json::Value =
        std::fs::read(&p).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_else(|| serde_json::json!({}));
    v["daemon"] = serde_json::Value::Bool(on);
    std::fs::create_dir_all(p.parent().unwrap())?;
    std::fs::write(p, serde_json::to_vec_pretty(&v)?)
}

fn config_path() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("flux").join("config.json"))
}
