//! Role-based access control for repositories.
//!
//! A principal's role on a repository is the highest of:
//! * `admin` for site admins, the owning user, and owners of the owning
//!   organization (and for everyone in `open` mode);
//! * the organization's base role, for its members;
//! * roles granted directly (collaborators) or through teams;
//! * `read` from visibility: `public` (anyone the server lets in) or
//!   `internal` (any signed-in user);
//! * the repository's signed-in role (`write` for repositories created
//!   before ownership existed, so they behave as before).
//!
//! The result is capped by the token's scopes. Roles are cached per
//! (user, repository) and the cache is dropped on any change to accounts,
//! memberships, grants or visibility.

use std::sync::Arc;

use flux_store::{RepoAccess, RepoId};

use crate::auth::Principal;
use crate::core::{Flux, FluxError, FluxResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum Role {
    #[default]
    None,
    /// Clone, browse, comment.
    Read,
    /// Also manage reviews (close, reopen) opened by others.
    Triage,
    /// Also push, create and land changes and reviews.
    Write,
    /// Also edit repository settings (description, default branch).
    Maintain,
    /// Everything: access, visibility, path rules, protected-ref overrides.
    Admin,
}

pub const ROLES: [Role; 5] = [Role::Read, Role::Triage, Role::Write, Role::Maintain, Role::Admin];

impl Role {
    pub fn parse(s: &str) -> Option<Role> {
        Some(match s {
            "none" | "" => Role::None,
            "read" => Role::Read,
            "triage" => Role::Triage,
            "write" => Role::Write,
            "maintain" => Role::Maintain,
            "admin" => Role::Admin,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Role::None => "none",
            Role::Read => "read",
            Role::Triage => "triage",
            Role::Write => "write",
            Role::Maintain => "maintain",
            Role::Admin => "admin",
        }
    }

    /// What a role allows, for display.
    pub fn describe(self) -> &'static str {
        match self {
            Role::None => "No access",
            Role::Read => "Browse, clone and comment",
            Role::Triage => "Read, plus manage reviews",
            Role::Write => "Triage, plus push and land changes",
            Role::Maintain => "Write, plus repository settings",
            Role::Admin => "Full control, including access and visibility",
        }
    }
}

/// The most a token's scopes allow. Sessions and `all` tokens are uncapped.
pub fn scope_cap(scopes: &str) -> Role {
    let mut cap = Role::None;
    for s in scopes.split([' ', ',']).filter(|s| !s.is_empty()) {
        cap = cap.max(match s {
            "all" | "admin" => Role::Admin,
            "write" => Role::Write,
            "read" => Role::Read,
            _ => Role::None,
        });
    }
    cap
}

/// Repositories a token is bound to (`repo:<id>` scopes), if any.
pub fn repo_limits(scopes: &str) -> Option<Vec<i64>> {
    let ids: Vec<i64> = scopes.split([' ', ',']).filter_map(|s| s.strip_prefix("repo:")?.parse().ok()).collect();
    (!ids.is_empty()).then_some(ids)
}

/// Ref patterns a token may move (`ref:<glob>` scopes); `None` = any.
pub fn ref_limits(scopes: &str) -> Option<Vec<String>> {
    let pats: Vec<String> = scopes.split([' ', ',']).filter_map(|s| s.strip_prefix("ref:")).map(str::to_string).collect();
    (!pats.is_empty()).then_some(pats)
}

/// The role a review action needs. Authors may always close, reopen and
/// comment on their own reviews.
pub fn review_action_role(action: &str, is_author: bool) -> Role {
    match action {
        "comment" => Role::Read,
        "close" | "reopen" if is_author => Role::Read,
        "close" | "reopen" => Role::Triage,
        _ => Role::Write,
    }
}

pub const VISIBILITIES: [&str; 3] = ["public", "internal", "private"];

impl Flux {
    /// Ownership and visibility of a repository (cached).
    pub async fn repo_access(&self, repo: RepoId) -> FluxResult<Arc<RepoAccess>> {
        if let Some(a) = self.access_cache.get(&repo) {
            return Ok(a);
        }
        let a = Arc::new(self.meta.repo_access(repo).await?.ok_or_else(|| FluxError::NotFound("repository not found".into()))?);
        self.access_cache.insert(repo, a.clone());
        Ok(a)
    }

    /// The principal's effective role on a repository.
    pub async fn repo_role(&self, who: &Principal, repo: RepoId) -> FluxResult<Role> {
        // Repository-bound tokens (CI jobs, agents): exactly their scope on
        // their repositories, nothing elsewhere.
        if let Some(repos) = repo_limits(&who.scopes) {
            return Ok(if repos.contains(&repo.0) { scope_cap(&who.scopes) } else { Role::None });
        }
        if who.mode == crate::auth::AuthMode::Open {
            return Ok(Role::Admin);
        }
        let key = (who.user.as_ref().map_or(0, |u| u.id), repo);
        let role = match self.role_cache.get(&key) {
            Some(r) => r,
            None => {
                let r = self.compute_role(who, repo).await?;
                self.role_cache.insert(key, r);
                r
            }
        };
        Ok(role.min(scope_cap(&who.scopes)))
    }

    async fn compute_role(&self, who: &Principal, repo: RepoId) -> FluxResult<Role> {
        let access = self.repo_access(repo).await?;
        let mut role = match access.visibility.as_str() {
            "public" if who.can_read() => Role::Read,
            "internal" if who.user.is_some() => Role::Read,
            _ => Role::None,
        };
        let Some(user) = &who.user else { return Ok(role) };
        if user.is_admin || (access.owner_kind == "user" && access.owner_id == user.id) {
            return Ok(Role::Admin);
        }
        role = role.max(Role::parse(&access.signed_in_role).unwrap_or_default());
        let facts = self.meta.authz_facts(repo, user.id).await?;
        match facts.org_role.as_deref() {
            Some("owner") => return Ok(Role::Admin),
            Some(_) => role = role.max(facts.org_base_role.as_deref().and_then(Role::parse).unwrap_or_default()),
            None => {}
        }
        for g in &facts.grants {
            role = role.max(Role::parse(g).unwrap_or_default());
        }
        Ok(role)
    }

    /// Require at least `need` on `repo`. Callers without read access to a
    /// non-public repository are told it does not exist.
    pub async fn authorize(&self, who: &Principal, repo: RepoId, need: Role) -> FluxResult<Role> {
        let role = self.repo_role(who, repo).await?;
        if role >= need {
            return Ok(role);
        }
        if role == Role::None {
            if who.user.is_none() && who.mode != crate::auth::AuthMode::Open {
                // Signing in might help; don't reveal whether the repository exists.
                return Err(FluxError::Unauthenticated("sign in to access this repository".into()));
            }
            return Err(FluxError::NotFound("repository not found".into()));
        }
        Err(FluxError::PermissionDenied(format!(
            "{} needs {} access to do that (has {})",
            who.name(),
            need.as_str(),
            role.as_str()
        )))
    }

    /// Drop cached roles after any change to accounts, memberships, grants
    /// or visibility.
    pub fn authz_changed(&self) {
        self.role_cache.invalidate_all();
        self.access_cache.invalidate_all();
        self.token_cache.invalidate_all();
    }

    /// Record a security-relevant event (best effort).
    pub async fn audit(&self, who: &Principal, action: &str, target: &str, detail: &str) {
        if let Err(e) = self.meta.audit_append(who.name(), action, target, detail).await {
            tracing::warn!(error = %e, action, "audit log write failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_order_and_scopes() {
        assert!(
            Role::Admin > Role::Maintain
                && Role::Maintain > Role::Write
                && Role::Write > Role::Triage
                && Role::Triage > Role::Read
        );
        assert_eq!(scope_cap("read"), Role::Read);
        assert_eq!(scope_cap("read write"), Role::Write);
        assert_eq!(scope_cap("all"), Role::Admin);
        assert_eq!(scope_cap(""), Role::None);
        assert_eq!(scope_cap("read repo:7"), Role::Read);
        assert_eq!(repo_limits("read repo:7 repo:9"), Some(vec![7, 9]));
        assert_eq!(repo_limits("write"), None);
        assert_eq!(ref_limits("write ref:agent/*"), Some(vec!["agent/*".to_string()]));
        for r in ROLES {
            assert_eq!(Role::parse(r.as_str()), Some(r));
        }
    }
}
