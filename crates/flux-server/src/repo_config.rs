//! Per-repository settings stored as JSON in the metadata store: the
//! description shown in the UI, the default branch, and path ACL rules.

use std::sync::Arc;

use flux_store::RepoId;
use serde::{Deserialize, Serialize};

use crate::core::{Flux, FluxError, FluxResult};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RepoConfig {
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub default_branch: Option<String>,
    /// Path ACL rules (SPEC §12.2). The most specific matching rule wins.
    #[serde(default)]
    pub acl: Vec<AclRule>,
}

/// Who may read and write under a path prefix. Principals are user names,
/// `"*"` (anyone who can access the repository) or `"admins"`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AclRule {
    pub path: String,
    #[serde(default)]
    pub read: Vec<String>,
    #[serde(default)]
    pub write: Vec<String>,
}

impl Flux {
    pub async fn repo_config(&self, repo: RepoId) -> FluxResult<Arc<RepoConfig>> {
        if let Some(c) = self.repo_configs.get(&repo) {
            return Ok(c);
        }
        let raw = self.meta.list_repos().await?.into_iter().find(|r| r.id == repo).map(|r| r.config).unwrap_or_default();
        let cfg: RepoConfig = serde_json::from_str(&raw).unwrap_or_default();
        let cfg = Arc::new(cfg);
        self.repo_configs.insert(repo, cfg.clone());
        Ok(cfg)
    }

    pub async fn set_repo_config(&self, repo: RepoId, cfg: &RepoConfig) -> FluxResult<()> {
        for r in &cfg.acl {
            if r.path.trim_matches('/').is_empty() {
                return Err(FluxError::InvalidArgument("ACL rules need a path prefix".into()));
            }
        }
        let raw = serde_json::to_string(cfg).map_err(|e| FluxError::Internal(e.into()))?;
        self.meta.set_repo_config(repo, &raw).await?;
        self.repo_configs.insert(repo, Arc::new(cfg.clone()));
        Ok(())
    }
}
