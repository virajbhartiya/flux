//! Accounts, sessions, tokens, organizations, teams and repository access
//! management. Every operation checks the caller's rights here, so the web
//! UI, gRPC and the CLI share one policy, and every change is audited.

use std::str::FromStr;

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString, rand_core::OsRng};
use flux_store::{Account, AccountUpdate, NewAccount, Org, RepoId, RepoInfo, Team, TokenInfo, User};

use crate::auth::{Principal, hash_token, new_token, now_ms};
use crate::core::{Flux, FluxError, FluxResult};
use crate::rbac::Role;

/// Who may create an account. The first account on a server is always
/// allowed and becomes a site admin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SignupPolicy {
    /// Only site admins create accounts.
    #[default]
    Closed,
    /// Anyone may sign up.
    Open,
}

impl FromStr for SignupPolicy {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "closed" => Ok(Self::Closed),
            "open" => Ok(Self::Open),
            o => Err(format!("unknown signup policy {o:?} (open | closed)")),
        }
    }
}

/// Web sessions last this long.
pub const SESSION_DAYS: i64 = 30;
const DAY_MS: i64 = 86_400_000;
/// Failed sign-ins allowed per account and client in 15 minutes (five
/// times as many per client across accounts).
const MAX_LOGIN_FAILURES: u32 = 10;
const MIN_PASSWORD: usize = 10;

/// Names that would collide with routes or confuse people.
const RESERVED: &[&str] =
    &["-", "admin", "api", "login", "logout", "new", "orgs", "settings", "signup", "static", "users", "anonymous", "flux"];

/// A valid user, organization or team name.
pub fn valid_name(name: &str) -> bool {
    valid_repo_name(name) && !RESERVED.contains(&name.to_ascii_lowercase().as_str())
}

/// Repository names may use reserved words (`acme/api` is fine).
pub fn valid_repo_name(name: &str) -> bool {
    (1..=100).contains(&name.len())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        && !name.starts_with(['-', '.'])
        && !name.ends_with('.')
}

fn check_name(kind: &str, name: &str) -> FluxResult<()> {
    let ok = if kind == "repository" { valid_repo_name(name) } else { valid_name(name) && name.len() <= 39 };
    if ok {
        Ok(())
    } else {
        Err(FluxError::InvalidArgument(format!(
            "invalid {kind} name {name:?}: use 1-39 letters, digits, '-', '_' or '.', not starting with '-' or '.'"
        )))
    }
}

async fn hash_password(password: String) -> FluxResult<String> {
    if password.chars().count() < MIN_PASSWORD {
        return Err(FluxError::InvalidArgument(format!("passwords need at least {MIN_PASSWORD} characters")));
    }
    tokio::task::spawn_blocking(move || {
        let salt = SaltString::generate(&mut OsRng);
        argon2::Argon2::default().hash_password(password.as_bytes(), &salt).map(|h| h.to_string())
    })
    .await
    .map_err(|e| FluxError::Internal(e.into()))?
    .map_err(|e| FluxError::Internal(anyhow::anyhow!("password hashing: {e}")))
}

async fn verify_password(password: String, phc: String) -> bool {
    tokio::task::spawn_blocking(move || {
        PasswordHash::new(&phc).is_ok_and(|h| argon2::Argon2::default().verify_password(password.as_bytes(), &h).is_ok())
    })
    .await
    .unwrap_or(false)
}

fn signed_in(who: &Principal) -> FluxResult<&User> {
    who.user.as_ref().ok_or_else(|| FluxError::Unauthenticated("sign in first".into()))
}

fn site_admin(who: &Principal) -> bool {
    who.mode == crate::auth::AuthMode::Open || who.user.as_ref().is_some_and(|u| u.is_admin)
}

impl Flux {
    // ───────────── repositories ─────────────

    /// Repositories the principal can read, by name.
    pub async fn readable_repos(&self, who: &Principal) -> FluxResult<Vec<RepoInfo>> {
        let mut out = Vec::new();
        for r in self.meta.list_repos().await? {
            if self.repo_role(who, r.id).await? >= Role::Read {
                out.push(r);
            }
        }
        Ok(out)
    }

    /// Create `name` (server-wide; site admins) or `owner/name` (your own,
    /// or an organization you own). Owned repositories are private unless
    /// another visibility is given.
    pub async fn create_repo_as(&self, who: &Principal, name: &str, visibility: Option<&str>) -> FluxResult<RepoId> {
        if let Some(v) = visibility
            && !crate::rbac::VISIBILITIES.contains(&v)
        {
            return Err(FluxError::InvalidArgument(format!("unknown visibility {v:?} (public | internal | private)")));
        }
        let id = match name.split_once('/') {
            None => {
                if !site_admin(who) {
                    return Err(FluxError::PermissionDenied(
                        "only site admins create server-wide repositories; use owner/name for your own".into(),
                    ));
                }
                check_name("repository", name)?;
                self.meta.create_repo_owned(name, "", 0, visibility.unwrap_or("public"), "write").await?
            }
            Some((owner, repo)) => {
                check_name("repository", repo)?;
                let (kind, owner_id) = if let Some(org) = self.meta.get_org(owner).await? {
                    if !self.is_org_owner(who, &org).await? {
                        return Err(FluxError::PermissionDenied(format!("only owners of {owner} can create its repositories")));
                    }
                    ("org", org.id)
                } else if let Some((acct, _)) = self.meta.get_account(owner).await?.filter(|(a, _)| a.user.name == owner) {
                    if who.user.as_ref().map(|u| u.id) != Some(acct.user.id) && !site_admin(who) {
                        return Err(FluxError::PermissionDenied(format!(
                            "you can only create repositories under your own name, not {owner}"
                        )));
                    }
                    ("user", acct.user.id)
                } else {
                    return Err(FluxError::NotFound(format!("no user or organization named {owner:?}")));
                };
                self.meta.create_repo_owned(name, kind, owner_id, visibility.unwrap_or("private"), "none").await?
            }
        };
        self.audit(who, "repo.create", name, visibility.unwrap_or("")).await;
        Ok(id)
    }

    pub async fn set_visibility(&self, who: &Principal, repo: RepoId, name: &str, visibility: &str) -> FluxResult<()> {
        self.authorize(who, repo, Role::Admin).await?;
        if !crate::rbac::VISIBILITIES.contains(&visibility) {
            return Err(FluxError::InvalidArgument(format!("unknown visibility {visibility:?}")));
        }
        let access = self.repo_access(repo).await?;
        self.meta.set_repo_visibility(repo, visibility, &access.signed_in_role).await?;
        self.authz_changed();
        self.audit(who, "repo.visibility", name, visibility).await;
        Ok(())
    }

    /// Grant (or with `None`, revoke) a role on a repository to a user or a
    /// team (`org/team` or a team name in the owning organization).
    pub async fn set_grant(
        &self,
        who: &Principal,
        repo: RepoId,
        repo_name: &str,
        kind: &str,
        subject: &str,
        role: Option<Role>,
    ) -> FluxResult<()> {
        self.authorize(who, repo, Role::Admin).await?;
        let subject_id = match kind {
            "user" => self.account_named(subject).await?.user.id,
            "team" => {
                let access = self.repo_access(repo).await?;
                let (org_name, team_name) = match subject.split_once('/') {
                    Some((o, t)) => (o.to_string(), t.to_string()),
                    None if access.owner_kind == "org" => {
                        let org = self
                            .meta
                            .get_org_by_id(access.owner_id)
                            .await?
                            .ok_or_else(|| FluxError::NotFound("organization".into()))?;
                        (org.name, subject.to_string())
                    }
                    None => return Err(FluxError::InvalidArgument("name the team as org/team".into())),
                };
                self.team_named(&org_name, &team_name).await?.1.id
            }
            _ => return Err(FluxError::InvalidArgument(format!("grant to a user or a team, not {kind:?}"))),
        };
        self.meta.set_repo_grant(repo, kind, subject_id, role.filter(|r| *r != Role::None).map(Role::as_str)).await?;
        self.authz_changed();
        self.audit(who, "repo.grant", repo_name, &format!("{kind} {subject} {}", role.map_or("none", Role::as_str))).await;
        Ok(())
    }

    // ───────────── accounts ─────────────

    async fn account_named(&self, name: &str) -> FluxResult<Account> {
        self.meta
            .get_account(name)
            .await?
            .map(|(a, _)| a)
            .filter(|a| a.user.name == name)
            .ok_or_else(|| FluxError::NotFound(format!("no user named {name:?}")))
    }

    /// Self-service sign-up (or the first account, which becomes an admin).
    pub async fn sign_up(&self, name: &str, email: Option<&str>, display_name: &str, password: &str) -> FluxResult<User> {
        let first = self.meta.user_count().await? == 0;
        if !first && self.signup != SignupPolicy::Open {
            return Err(FluxError::PermissionDenied("sign-up is closed on this server; ask an admin for an account".into()));
        }
        self.new_account(name, email, display_name, Some(password), first).await
    }

    /// An admin creates an account (optionally without a password: the
    /// user then signs in with tokens only).
    pub async fn admin_create_account(
        &self,
        who: &Principal,
        name: &str,
        email: Option<&str>,
        password: Option<&str>,
        is_admin: bool,
    ) -> FluxResult<User> {
        if !site_admin(who) {
            return Err(FluxError::PermissionDenied("only site admins create accounts".into()));
        }
        let u = self.new_account(name, email, "", password, is_admin).await?;
        self.audit(who, "user.create", name, if is_admin { "admin" } else { "" }).await;
        Ok(u)
    }

    async fn new_account(
        &self,
        name: &str,
        email: Option<&str>,
        display_name: &str,
        password: Option<&str>,
        is_admin: bool,
    ) -> FluxResult<User> {
        check_name("user", name)?;
        let email = email.map(str::trim).filter(|e| !e.is_empty());
        if let Some(e) = email
            && (!e.contains('@') || e.len() > 254)
        {
            return Err(FluxError::InvalidArgument("that email address doesn't look right".into()));
        }
        let password_hash = match password {
            Some(p) => Some(hash_password(p.to_string()).await?),
            None => None,
        };
        let u = self
            .meta
            .create_account(&NewAccount {
                name: name.to_string(),
                email: email.map(str::to_lowercase),
                display_name: display_name.trim().to_string(),
                password_hash,
                is_admin,
            })
            .await?;
        self.meta.audit_append(name, "user.signup", name, if is_admin { "admin" } else { "" }).await?;
        Ok(u)
    }

    /// Check a password and open a web session. Throttled per account and
    /// per client; the error never says which part was wrong.
    pub async fn password_login(&self, name_or_email: &str, password: &str, client: &str) -> FluxResult<(User, String)> {
        // Per (account, client) and per client: guessing is throttled, but
        // nobody can lock a user out from somewhere else.
        let keys = [format!("u:{}|{client}", name_or_email.to_lowercase()), format!("c:{client}")];
        let limits = [MAX_LOGIN_FAILURES, MAX_LOGIN_FAILURES * 5];
        if keys.iter().zip(limits).any(|(k, limit)| self.login_failures.get(k).unwrap_or(0) >= limit) {
            return Err(FluxError::PermissionDenied("too many failed sign-ins; try again in 15 minutes".into()));
        }
        let found = self.meta.get_account(name_or_email.trim()).await?;
        let ok = match &found {
            Some((acct, Some(phc))) if !acct.disabled => verify_password(password.to_string(), phc.clone()).await,
            _ => {
                // Same cost either way, so timing doesn't reveal accounts.
                let _ = verify_password(password.to_string(), DUMMY_HASH.to_string()).await;
                false
            }
        };
        let Some((acct, _)) = found.filter(|_| ok) else {
            for k in &keys {
                self.login_failures.insert(k.clone(), self.login_failures.get(k).unwrap_or(0) + 1);
            }
            self.meta.audit_append(name_or_email, "user.login_failed", name_or_email, client).await?;
            return Err(FluxError::Unauthenticated("incorrect name or password".into()));
        };
        self.login_failures.invalidate(&keys[0]);
        let token = new_token();
        let expires = now_ms() + SESSION_DAYS * DAY_MS;
        self.meta
            .add_token_ext(acct.user.id, &hash_token(&token), &format!("web session ({client})"), "session", "all", Some(expires))
            .await?;
        self.meta.audit_append(&acct.user.name, "user.login", &acct.user.name, client).await?;
        Ok((acct.user, token))
    }

    /// End a session (or revoke the presented token).
    pub async fn logout(&self, token: &str) -> FluxResult<()> {
        let h = hash_token(token);
        self.meta.revoke_token_hash(&h).await?;
        self.token_cache.invalidate(&h);
        Ok(())
    }

    pub async fn change_password(&self, who: &Principal, old: &str, new: &str) -> FluxResult<()> {
        let me = signed_in(who)?;
        let (_, phc) = self.meta.get_account(&me.name).await?.ok_or_else(|| FluxError::NotFound("account".into()))?;
        if let Some(phc) = phc
            && !verify_password(old.to_string(), phc).await
        {
            return Err(FluxError::PermissionDenied("the current password is wrong".into()));
        }
        let hash = hash_password(new.to_string()).await?;
        self.meta.update_account(me.id, &AccountUpdate { password_hash: Some(Some(hash)), ..Default::default() }).await?;
        self.audit(who, "user.password", &me.name, "").await;
        Ok(())
    }

    pub async fn update_profile(&self, who: &Principal, display_name: &str, email: Option<&str>) -> FluxResult<()> {
        let me = signed_in(who)?;
        let email = email.map(str::trim).filter(|e| !e.is_empty()).map(str::to_lowercase);
        self.meta
            .update_account(
                me.id,
                &AccountUpdate { display_name: Some(display_name.trim().to_string()), email: Some(email), ..Default::default() },
            )
            .await?;
        Ok(())
    }

    /// Site admins: promote, demote, disable or enable an account.
    pub async fn admin_update_account(
        &self,
        who: &Principal,
        name: &str,
        is_admin: Option<bool>,
        disabled: Option<bool>,
    ) -> FluxResult<()> {
        if !site_admin(who) {
            return Err(FluxError::PermissionDenied("only site admins manage accounts".into()));
        }
        let acct = self.account_named(name).await?;
        if who.user.as_ref().is_some_and(|u| u.id == acct.user.id) && (is_admin == Some(false) || disabled == Some(true)) {
            return Err(FluxError::FailedPrecondition("you can't demote or disable your own account".into()));
        }
        self.meta.update_account(acct.user.id, &AccountUpdate { is_admin, disabled, ..Default::default() }).await?;
        self.authz_changed();
        self.audit(who, "user.update", name, &format!("admin={is_admin:?} disabled={disabled:?}")).await;
        Ok(())
    }

    /// A personal access token. `scopes`: `read`, `write` or `all`.
    pub async fn create_api_token(
        &self,
        who: &Principal,
        label: &str,
        scopes: &str,
        expires_days: Option<i64>,
    ) -> FluxResult<String> {
        let me = signed_in(who)?;
        if !["read", "write", "all"].contains(&scopes) {
            return Err(FluxError::InvalidArgument("scopes: read, write or all".into()));
        }
        // A token can't grant more than the credential creating it.
        if crate::rbac::scope_cap(scopes) > crate::rbac::scope_cap(&who.scopes) {
            return Err(FluxError::PermissionDenied("this token can't create a token with broader scopes".into()));
        }
        let label = if label.trim().is_empty() { "token" } else { label.trim() };
        let token = new_token();
        let expires = expires_days.filter(|d| *d > 0).map(|d| now_ms() + d * DAY_MS);
        self.meta.add_token_ext(me.id, &hash_token(&token), label, "api", scopes, expires).await?;
        self.audit(who, "token.create", &me.name, &format!("{label} ({scopes})")).await;
        Ok(token)
    }

    pub async fn my_tokens(&self, who: &Principal) -> FluxResult<Vec<TokenInfo>> {
        Ok(self.meta.list_tokens(signed_in(who)?.id).await?)
    }

    pub async fn revoke_my_token(&self, who: &Principal, id: &str) -> FluxResult<()> {
        let me = signed_in(who)?;
        if !self.meta.revoke_token(me.id, id).await? {
            return Err(FluxError::NotFound("no such token".into()));
        }
        self.token_cache.invalidate_all();
        self.audit(who, "token.revoke", &me.name, id).await;
        Ok(())
    }

    // ───────────── organizations ─────────────

    pub async fn create_org(&self, who: &Principal, name: &str, display_name: &str) -> FluxResult<Org> {
        let me = signed_in(who)?;
        check_name("organization", name)?;
        let org = self.meta.create_org(name, display_name.trim(), me.id).await?;
        self.authz_changed();
        self.audit(who, "org.create", name, "").await;
        Ok(org)
    }

    pub async fn org_named(&self, name: &str) -> FluxResult<Org> {
        self.meta.get_org(name).await?.ok_or_else(|| FluxError::NotFound(format!("no organization named {name:?}")))
    }

    /// The principal's role in an organization: `owner`, `member` or none.
    pub async fn org_role(&self, who: &Principal, org: &Org) -> FluxResult<Option<String>> {
        let Some(me) = &who.user else { return Ok(None) };
        Ok(self.meta.org_members(org.id).await?.into_iter().find(|(u, _)| u.id == me.id).map(|(_, r)| r))
    }

    pub async fn is_org_owner(&self, who: &Principal, org: &Org) -> FluxResult<bool> {
        Ok(site_admin(who) || self.org_role(who, org).await?.as_deref() == Some("owner"))
    }

    async fn require_org_owner(&self, who: &Principal, org: &Org) -> FluxResult<()> {
        if self.is_org_owner(who, org).await? {
            Ok(())
        } else {
            Err(FluxError::PermissionDenied(format!("only owners of {} can do that", org.name)))
        }
    }

    pub async fn update_org(
        &self,
        who: &Principal,
        org: &Org,
        display_name: &str,
        description: &str,
        base_role: &str,
    ) -> FluxResult<()> {
        self.require_org_owner(who, org).await?;
        if !["none", "read", "write"].contains(&base_role) {
            return Err(FluxError::InvalidArgument("base role: none, read or write".into()));
        }
        self.meta.update_org(org.id, display_name.trim(), description.trim(), base_role).await?;
        self.authz_changed();
        self.audit(who, "org.update", &org.name, &format!("base_role={base_role}")).await;
        Ok(())
    }

    /// Add, change or (with `None`) remove a member. An organization always
    /// keeps at least one owner.
    pub async fn set_org_member(&self, who: &Principal, org: &Org, user: &str, role: Option<&str>) -> FluxResult<()> {
        let me_leaving = role.is_none() && who.user.as_ref().is_some_and(|u| u.name == user);
        if !me_leaving {
            self.require_org_owner(who, org).await?;
        }
        if let Some(r) = role
            && !["owner", "member"].contains(&r)
        {
            return Err(FluxError::InvalidArgument("role: owner or member".into()));
        }
        let acct = self.account_named(user).await?;
        let members = self.meta.org_members(org.id).await?;
        let is_owner = members.iter().any(|(u, r)| u.id == acct.user.id && r == "owner");
        let owners = members.iter().filter(|(_, r)| r == "owner").count();
        if is_owner && owners == 1 && role != Some("owner") {
            return Err(FluxError::FailedPrecondition(format!("{} needs at least one owner", org.name)));
        }
        self.meta.set_org_member(org.id, acct.user.id, role).await?;
        self.authz_changed();
        self.audit(who, "org.member", &org.name, &format!("{user} {}", role.unwrap_or("removed"))).await;
        Ok(())
    }

    // ───────────── teams ─────────────

    pub async fn team_named(&self, org: &str, team: &str) -> FluxResult<(Org, Team)> {
        let org = self.org_named(org).await?;
        let t = self
            .meta
            .list_teams(org.id)
            .await?
            .into_iter()
            .find(|t| t.name == team)
            .ok_or_else(|| FluxError::NotFound(format!("no team {team:?} in {}", org.name)))?;
        Ok((org, t))
    }

    pub async fn create_team(&self, who: &Principal, org: &Org, name: &str, description: &str) -> FluxResult<Team> {
        self.require_org_owner(who, org).await?;
        check_name("team", name)?;
        let t = self.meta.create_team(org.id, name, description.trim()).await?;
        self.audit(who, "team.create", &format!("{}/{name}", org.name), "").await;
        Ok(t)
    }

    pub async fn delete_team(&self, who: &Principal, org: &Org, team: &Team) -> FluxResult<()> {
        self.require_org_owner(who, org).await?;
        self.meta.delete_team(team.id).await?;
        self.authz_changed();
        self.audit(who, "team.delete", &format!("{}/{}", org.name, team.name), "").await;
        Ok(())
    }

    /// Team members must belong to the organization.
    pub async fn set_team_member(&self, who: &Principal, org: &Org, team: &Team, user: &str, member: bool) -> FluxResult<()> {
        self.require_org_owner(who, org).await?;
        let acct = self.account_named(user).await?;
        if member && !self.meta.org_members(org.id).await?.iter().any(|(u, _)| u.id == acct.user.id) {
            return Err(FluxError::FailedPrecondition(format!("{user} is not a member of {}", org.name)));
        }
        self.meta.set_team_member(team.id, acct.user.id, member).await?;
        self.authz_changed();
        self.audit(
            who,
            "team.member",
            &format!("{}/{}", org.name, team.name),
            &format!("{user} {}", if member { "added" } else { "removed" }),
        )
        .await;
        Ok(())
    }
}

/// An argon2id hash of a random password, verified against when an account
/// doesn't exist so failed sign-ins take the same time.
const DUMMY_HASH: &str = "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHRzb21lc2FsdA$J6Y1mWb9cGHRk1QZ1oRGZ9u3hmyb1mn4yeQHtPLkYDA";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert!(valid_name("alice") && valid_name("acme-corp") && valid_name("a.b_c"));
        assert!(!valid_name("") && !valid_name("-x") && !valid_name(".x") && !valid_name("x.") && !valid_name("a/b"));
        assert!(!valid_name("settings") && !valid_name("Admin"));
        assert!(valid_repo_name("api") && !valid_name("api"));
    }
}
