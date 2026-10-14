//! Authentication and authorization (SPEC §12, token flavour).
//!
//! Users authenticate with bearer tokens (`flx_` + 64 hex chars). Only a
//! SHA-256 of each token is stored. Three modes:
//! * `open`: no authentication; everyone may read and write (single-user or
//!   trusted networks, the default for `--data-dir` setups);
//! * `token`: anyone may read; writes need a valid token;
//! * `private`: every request needs a valid token.
//!
//! Admin operations (creating repositories) require an admin user in the
//! `token` and `private` modes.

use std::str::FromStr;

use flux_store::User;
use sha2::Digest as _;

use crate::core::{Flux, FluxError, FluxResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AuthMode {
    #[default]
    Open,
    Token,
    Private,
}

impl FromStr for AuthMode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "open" => Ok(Self::Open),
            "token" => Ok(Self::Token),
            "private" => Ok(Self::Private),
            other => Err(format!("unknown auth mode {other:?} (open | token | private)")),
        }
    }
}

impl AuthMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Token => "token",
            Self::Private => "private",
        }
    }
}

/// Who is making a request. `user = None` is anonymous.
#[derive(Debug, Clone, Default)]
pub struct Principal {
    pub user: Option<User>,
    pub mode: AuthMode,
}

impl Principal {
    pub fn name(&self) -> &str {
        self.user.as_ref().map(|u| u.name.as_str()).unwrap_or("anonymous")
    }

    pub fn can_read(&self) -> bool {
        self.mode != AuthMode::Private || self.user.is_some()
    }

    pub fn can_write(&self) -> bool {
        self.mode == AuthMode::Open || self.user.is_some()
    }

    pub fn is_admin(&self) -> bool {
        self.mode == AuthMode::Open || self.user.as_ref().is_some_and(|u| u.is_admin)
    }

    pub fn require_read(&self) -> FluxResult<()> {
        if self.can_read() { Ok(()) } else { Err(FluxError::Unauthenticated("this server requires a token".into())) }
    }

    pub fn require_write(&self) -> FluxResult<()> {
        if self.can_write() {
            Ok(())
        } else {
            Err(FluxError::Unauthenticated("writes require a token (flux login)".into()))
        }
    }

    pub fn require_admin(&self) -> FluxResult<()> {
        if self.is_admin() {
            Ok(())
        } else if self.user.is_none() {
            Err(FluxError::Unauthenticated("admin operations require a token".into()))
        } else {
            Err(FluxError::PermissionDenied(format!("{} is not an admin", self.name())))
        }
    }
}

pub fn hash_token(token: &str) -> [u8; 32] {
    sha2::Sha256::digest(token.trim().as_bytes()).into()
}

/// A fresh random token.
pub fn new_token() -> String {
    let mut b = [0u8; 32];
    getrandom::fill(&mut b).expect("OS random number generator");
    format!("flx_{}", hex::encode(b))
}

impl Flux {
    /// Resolve a bearer token (if any) into a principal. Invalid tokens are
    /// an error even in open mode, so typos don't silently go anonymous.
    pub async fn principal(&self, token: Option<&str>) -> FluxResult<Principal> {
        let mode = self.auth_mode;
        let Some(token) = token.map(str::trim).filter(|t| !t.is_empty()) else {
            return Ok(Principal { user: None, mode });
        };
        let h = hash_token(token);
        if let Some(u) = self.token_cache.get(&h) {
            return Ok(Principal { user: Some(u), mode });
        }
        match self.meta.user_for_token(&h).await? {
            Some(u) => {
                self.token_cache.insert(h, u.clone());
                Ok(Principal { user: Some(u), mode })
            }
            None => Err(FluxError::Unauthenticated("invalid or revoked token".into())),
        }
    }

    /// Create a user (if needed) and issue a token for them.
    pub async fn issue_token(&self, user: &str, admin: bool, label: &str) -> FluxResult<(User, String)> {
        if user.is_empty() || !user.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)) {
            return Err(FluxError::InvalidArgument(format!("invalid user name {user:?}")));
        }
        let u = match self.meta.get_user(user).await? {
            Some(u) => u,
            None => self.meta.create_user(user, admin).await?,
        };
        let token = new_token();
        self.meta.add_token(u.id, &hash_token(&token), label).await?;
        Ok((u, token))
    }

    pub async fn revoke_tokens(&self, user: &str) -> FluxResult<u64> {
        let u = self.meta.get_user(user).await?.ok_or_else(|| FluxError::NotFound(format!("user {user:?} not found")))?;
        let n = self.meta.revoke_tokens(u.id).await?;
        self.token_cache.invalidate_all();
        Ok(n)
    }
}
