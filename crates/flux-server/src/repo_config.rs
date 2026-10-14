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

impl RepoConfig {
    /// The most specific rule covering `path` (component-wise prefix).
    pub fn rule_for(&self, path: &[u8]) -> Option<&AclRule> {
        self.acl
            .iter()
            .filter(|r| {
                let p = r.path.trim_matches('/').as_bytes();
                path.starts_with(p) && (path.len() == p.len() || path[p.len()] == b'/')
            })
            .max_by_key(|r| r.path.trim_matches('/').len())
    }

    /// May `who` read (or write) `path`? Admins and paths without a rule
    /// follow repository access.
    pub fn allows(&self, who: &crate::auth::Principal, path: &[u8], write: bool) -> bool {
        if self.acl.is_empty() || who.is_admin() {
            return true;
        }
        let Some(rule) = self.rule_for(path) else { return true };
        let list = if write { &rule.write } else { &rule.read };
        list.iter().any(|p| p == "*" || (p == "admins" && who.is_admin()) || who.user.as_ref().is_some_and(|u| &u.name == p))
    }

    /// Is anything under directory `dir` readable? (A restricted directory
    /// may still contain an explicitly opened subtree.)
    pub fn dir_visible(&self, who: &crate::auth::Principal, dir: &[u8]) -> bool {
        self.allows(who, dir, false)
            || self.acl.iter().any(|r| {
                let p = r.path.trim_matches('/').as_bytes();
                p.starts_with(dir) && p.get(dir.len()) == Some(&b'/') && self.allows(who, p, false)
            })
    }

    pub fn has_rules(&self) -> bool {
        !self.acl.is_empty()
    }
}

impl Flux {
    /// Reject writes to paths the principal may not write.
    pub async fn check_write_paths(&self, repo: RepoId, who: &crate::auth::Principal, paths: Vec<Vec<u8>>) -> FluxResult<()> {
        let cfg = self.repo_config(repo).await?;
        if !cfg.has_rules() {
            return Ok(());
        }
        for p in &paths {
            if !cfg.allows(who, p, true) {
                return Err(FluxError::PermissionDenied(format!("{} may not write {}", who.name(), String::from_utf8_lossy(p))));
            }
        }
        Ok(())
    }

    /// Landing writes every path the stack touches onto the target.
    pub async fn check_land_paths(
        &self,
        repo: RepoId,
        who: &crate::auth::Principal,
        top: &flux_core::ChangesetId,
        target: &[u8],
    ) -> FluxResult<()> {
        if !self.repo_config(repo).await?.has_rules() {
            return Ok(());
        }
        let stack = match self.meta.get_ref(repo, target).await? {
            Some(h) => self.stack_between(repo, top, &h.target).await?,
            None => vec![self.load_one_arc(repo, top).await?],
        };
        let paths: Vec<Vec<u8>> =
            stack.iter().flat_map(|l| l.changeset.file_changes.keys().map(|p| p.as_bytes().to_vec())).collect();
        self.check_write_paths(repo, who, paths).await
    }

    pub async fn check_read_path(&self, repo: RepoId, who: &crate::auth::Principal, path: &[u8]) -> FluxResult<()> {
        if self.repo_config(repo).await?.allows(who, path, false) {
            Ok(())
        } else {
            Err(FluxError::NotFound(format!("{} not found", String::from_utf8_lossy(path))))
        }
    }

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{AuthMode, Principal};
    use flux_store::User;

    #[test]
    fn most_specific_rule_wins() {
        let cfg = RepoConfig {
            acl: vec![
                AclRule { path: "infra".into(), read: vec!["*".into()], write: vec!["ops".into()] },
                AclRule { path: "infra/secrets".into(), read: vec!["ops".into()], write: vec!["ops".into()] },
            ],
            ..Default::default()
        };
        let who =
            |name: &str| Principal { user: Some(User { id: 1, name: name.into(), is_admin: false }), mode: AuthMode::Token };
        assert!(cfg.allows(&who("dev"), b"infra/dns.tf", false));
        assert!(!cfg.allows(&who("dev"), b"infra/dns.tf", true));
        assert!(!cfg.allows(&who("dev"), b"infra/secrets/key", false));
        assert!(cfg.allows(&who("ops"), b"infra/secrets/key", true));
        assert!(cfg.allows(&who("dev"), b"src/main.rs", true), "no rule: repository access");
        assert!(!cfg.allows(&who("dev"), b"infra-other", false) || cfg.rule_for(b"infra-other").is_none());
        assert!(cfg.dir_visible(&who("dev"), b"infra"));
    }
}
