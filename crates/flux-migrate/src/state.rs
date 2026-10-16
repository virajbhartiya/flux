//! Sync state kept next to the git mirror (`<repo>.state.json`): the last
//! sync time for incremental fetches, ETag-cached pages, users already
//! created, and the refs imported last time (to mirror deletions).

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::github::CachedPage;

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    /// `<flux server> <flux repo>` the state belongs to.
    pub target: String,
    /// GitHub time (RFC 3339) to pass as `since=` next time.
    pub since: Option<String>,
    pub etags: HashMap<String, CachedPage>,
    /// Placeholder accounts already sent: login → GitHub user id.
    pub users: BTreeMap<String, u64>,
    /// flux refs imported by the last sync: name → git SHA-1.
    pub refs: BTreeMap<String, String>,
}

/// `<repo>.git` → `<repo>.state.json`.
pub fn path_for(git_dir: &Path) -> PathBuf {
    let name = git_dir.file_name().map(|n| n.to_string_lossy().trim_end_matches(".git").to_string()).unwrap_or_default();
    git_dir.with_file_name(format!("{name}.state.json"))
}

impl State {
    /// The saved state for `target`, or a fresh one.
    pub fn load(path: &Path, target: &str) -> Self {
        let s: State = std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        if s.target == target { s } else { State { target: target.to_string(), ..Default::default() } }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(self)?).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_round_trips_per_target() {
        let dir = tempfile::tempdir().unwrap();
        let p = path_for(&dir.path().join("widget.git"));
        assert_eq!(p.file_name().unwrap(), "widget.state.json");
        let mut s = State::load(&p, "http://f a");
        s.since = Some("2024-01-01T00:00:00Z".into());
        s.refs.insert("main".into(), "ab".repeat(20));
        s.save(&p).unwrap();
        assert_eq!(State::load(&p, "http://f a").since.as_deref(), Some("2024-01-01T00:00:00Z"));
        assert!(State::load(&p, "http://f b").since.is_none(), "another target starts over");
    }
}
