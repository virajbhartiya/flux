//! SSH public keys: parsing, fingerprints, and account management.

use base64::Engine as _;
use flux_store::SshKey;
use sha2::Digest as _;

use crate::auth::Principal;
use crate::core::{Flux, FluxError, FluxResult};

const ALGORITHMS: [&str; 7] = [
    "ssh-ed25519",
    "ecdsa-sha2-nistp256",
    "ecdsa-sha2-nistp384",
    "ecdsa-sha2-nistp521",
    "ssh-rsa",
    "sk-ssh-ed25519@openssh.com",
    "sk-ecdsa-sha2-nistp256@openssh.com",
];

/// A parsed `authorized_keys`-style public key line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicKey {
    pub algorithm: String,
    pub blob: Vec<u8>,
    pub comment: String,
}

impl PublicKey {
    pub fn parse(line: &str) -> FluxResult<Self> {
        let mut parts = line.split_whitespace();
        let algorithm = parts.next().unwrap_or_default().to_string();
        if !ALGORITHMS.contains(&algorithm.as_str()) {
            return Err(FluxError::InvalidArgument(format!("unsupported key type {algorithm:?} (use ed25519, ecdsa or rsa)")));
        }
        let blob = base64::engine::general_purpose::STANDARD
            .decode(parts.next().unwrap_or_default())
            .map_err(|_| FluxError::InvalidArgument("the key is not valid base64".into()))?;
        // The blob starts with the length-prefixed algorithm name.
        let ok = blob.len() > 4 && {
            let n = u32::from_be_bytes([blob[0], blob[1], blob[2], blob[3]]) as usize;
            blob.get(4..4 + n) == Some(algorithm.as_bytes())
        };
        if !ok {
            return Err(FluxError::InvalidArgument("the key data does not match its type".into()));
        }
        if algorithm == "ssh-rsa" && blob.len() < 270 {
            return Err(FluxError::InvalidArgument("RSA keys need at least 2048 bits".into()));
        }
        Ok(Self { algorithm, blob, comment: parts.collect::<Vec<_>>().join(" ") })
    }

    /// `SHA256:<base64, unpadded>`, as `ssh-keygen -lf` prints it.
    pub fn fingerprint(&self) -> String {
        fingerprint(&self.blob)
    }

    pub fn openssh(&self) -> String {
        format!("{} {}", self.algorithm, base64::engine::general_purpose::STANDARD.encode(&self.blob))
    }
}

pub fn fingerprint(blob: &[u8]) -> String {
    format!("SHA256:{}", base64::engine::general_purpose::STANDARD_NO_PAD.encode(sha2::Sha256::digest(blob)))
}

impl Flux {
    pub async fn add_ssh_key(&self, who: &Principal, title: &str, key: &str) -> FluxResult<SshKey> {
        let user = who.user.as_ref().ok_or_else(|| FluxError::Unauthenticated("sign in first".into()))?;
        let k = PublicKey::parse(key.trim())?;
        let title = if title.trim().is_empty() { k.comment.clone() } else { title.trim().to_string() };
        let saved = self.meta.add_ssh_key(user.id, &title, &k.fingerprint(), &k.openssh()).await?;
        self.audit(who, "ssh_key.add", &user.name, &saved.fingerprint).await;
        Ok(saved)
    }

    pub async fn delete_ssh_key(&self, who: &Principal, id: i64) -> FluxResult<()> {
        let user = who.user.as_ref().ok_or_else(|| FluxError::Unauthenticated("sign in first".into()))?;
        if !self.meta.delete_ssh_key(user.id, id).await? {
            return Err(FluxError::NotFound("no such key".into()));
        }
        self.audit(who, "ssh_key.delete", &user.name, &id.to_string()).await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_fingerprints() {
        // A throwaway key generated with `ssh-keygen -t ed25519`.
        let line = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl test@flux";
        let k = PublicKey::parse(line).unwrap();
        assert_eq!(k.algorithm, "ssh-ed25519");
        assert_eq!(k.comment, "test@flux");
        assert!(k.fingerprint().starts_with("SHA256:"));
        assert_eq!(k.fingerprint().len(), 7 + 43);
        assert!(PublicKey::parse("ssh-dss AAAA").is_err());
        assert!(PublicKey::parse("ssh-ed25519 AAAAB3NzaC1yc2EAAAADAQABAAABAQ").is_err());
    }
}
