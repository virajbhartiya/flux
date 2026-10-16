//! Encrypted CI secrets (repository and organization scope).
//!
//! Values are sealed with ChaCha20-Poly1305 under the server's secret key,
//! with the scope and name as associated data so a ciphertext cannot be
//! moved to another secret. Values are write-only through every API: only
//! CI jobs receive them, and runners mask them in logs.

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};
use flux_store::RepoId;
use serde::Serialize;

use crate::auth::Principal;
use crate::core::{Flux, FluxError, FluxResult};
use crate::rbac::Role;

#[derive(Debug, Clone, Serialize)]
pub struct SecretInfo {
    pub name: String,
    pub updated_at: i64,
}

pub fn valid_secret_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 100
        && n.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        && !n.as_bytes()[0].is_ascii_digit()
        && !n.to_ascii_uppercase().starts_with("GITHUB_")
        && !n.to_ascii_uppercase().starts_with("FLUX_")
}

fn aad(scope: &str, id: i64, name: &str) -> Vec<u8> {
    format!("flux-secret/{scope}/{id}/{name}").into_bytes()
}

impl Flux {
    fn cipher(&self) -> FluxResult<ChaCha20Poly1305> {
        let key = self.settings.secret_key.ok_or_else(|| {
            FluxError::FailedPrecondition("secrets need a server secret key (flux-server --secret-key-file)".into())
        })?;
        Ok(ChaCha20Poly1305::new(&key.into()))
    }

    pub fn seal(&self, aad_data: &[u8], plain: &[u8]) -> FluxResult<Vec<u8>> {
        let mut nonce = [0u8; 12];
        getrandom::fill(&mut nonce).expect("OS random number generator");
        let ct = self
            .cipher()?
            .encrypt(Nonce::from_slice(&nonce), Payload { msg: plain, aad: aad_data })
            .map_err(|_| FluxError::Internal(anyhow::anyhow!("encryption failed")))?;
        let mut out = nonce.to_vec();
        out.extend_from_slice(&ct);
        Ok(out)
    }

    pub fn open_sealed(&self, aad_data: &[u8], sealed: &[u8]) -> FluxResult<Vec<u8>> {
        if sealed.len() < 12 {
            return Err(FluxError::Internal(anyhow::anyhow!("sealed value too short")));
        }
        self.cipher()?
            .decrypt(Nonce::from_slice(&sealed[..12]), Payload { msg: &sealed[12..], aad: aad_data })
            .map_err(|_| FluxError::Internal(anyhow::anyhow!("cannot decrypt (wrong server key?)")))
    }

    async fn secret_scope_admin(&self, who: &Principal, scope: (&str, i64)) -> FluxResult<()> {
        match scope.0 {
            "repo" => {
                self.authorize(who, RepoId(scope.1), Role::Admin).await?;
            }
            _ => {
                let org = self.meta.get_org_by_id(scope.1).await?.ok_or_else(|| FluxError::NotFound("organization".into()))?;
                if !self.is_org_owner(who, &org).await? {
                    return Err(FluxError::PermissionDenied("only organization owners manage its secrets".into()));
                }
            }
        }
        Ok(())
    }

    pub async fn put_secret(&self, who: &Principal, scope: (&str, i64), name: &str, value: &str) -> FluxResult<()> {
        self.secret_scope_admin(who, scope).await?;
        if !valid_secret_name(name) {
            return Err(FluxError::InvalidArgument(
                "secret names use letters, digits and _, do not start with a digit, GITHUB_ or FLUX_".into(),
            ));
        }
        if value.len() > 48 * 1024 {
            return Err(FluxError::InvalidArgument("secrets are limited to 48 KB".into()));
        }
        let sealed = self.seal(&aad(scope.0, scope.1, name), value.as_bytes())?;
        self.meta.put_secret(scope.0, scope.1, name, &sealed).await?;
        self.audit(who, "secret.put", &format!("{}:{}", scope.0, scope.1), name).await;
        Ok(())
    }

    pub async fn delete_secret(&self, who: &Principal, scope: (&str, i64), name: &str) -> FluxResult<()> {
        self.secret_scope_admin(who, scope).await?;
        if !self.meta.delete_secret(scope.0, scope.1, name).await? {
            return Err(FluxError::NotFound(format!("no secret named {name}")));
        }
        self.audit(who, "secret.delete", &format!("{}:{}", scope.0, scope.1), name).await;
        Ok(())
    }

    pub async fn list_secrets(&self, who: &Principal, scope: (&str, i64)) -> FluxResult<Vec<SecretInfo>> {
        self.secret_scope_admin(who, scope).await?;
        Ok(self
            .meta
            .secrets(scope.0, scope.1)
            .await?
            .into_iter()
            .map(|(name, _, updated_at)| SecretInfo { name, updated_at })
            .collect())
    }

    /// Decrypted secrets for a CI job: organization secrets, overridden by
    /// repository secrets of the same name.
    pub(crate) async fn job_secrets(&self, repo: RepoId) -> FluxResult<std::collections::BTreeMap<String, String>> {
        let mut out = std::collections::BTreeMap::new();
        if self.settings.secret_key.is_none() {
            return Ok(out);
        }
        let access = self.repo_access(repo).await?;
        let mut scopes = vec![];
        if access.owner_kind == "org" {
            scopes.push(("org", access.owner_id));
        }
        scopes.push(("repo", repo.0));
        for (kind, id) in scopes {
            for (name, sealed, _) in self.meta.secrets(kind, id).await? {
                match self.open_sealed(&aad(kind, id, &name), &sealed) {
                    Ok(v) => {
                        out.insert(name, String::from_utf8_lossy(&v).into_owned());
                    }
                    Err(e) => tracing::warn!(secret = %name, error = %e, "secret could not be decrypted"),
                }
            }
        }
        Ok(out)
    }
}
