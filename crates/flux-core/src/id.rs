//! Typed, domain-separated BLAKE3-256 identifiers (SPEC §2.1).

use std::fmt;
use std::str::FromStr;

use crate::error::{Error, Result};

/// Object kinds, used for hash domain separation and blob-store key layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ObjectKind {
    Content,
    Chunk,
    Tree,
    Changeset,
    Derived,
}

impl ObjectKind {
    /// BLAKE3 `derive_key` context string. Frozen for v1.
    pub const fn context(self) -> &'static str {
        match self {
            ObjectKind::Content => "flux 2026 content v1",
            ObjectKind::Chunk => "flux 2026 chunk v1",
            ObjectKind::Tree => "flux 2026 tree v1",
            ObjectKind::Changeset => "flux 2026 changeset v1",
            ObjectKind::Derived => "flux 2026 derived v1",
        }
    }

    pub const fn display_prefix(self) -> &'static str {
        match self {
            ObjectKind::Content => "c_",
            ObjectKind::Chunk => "k_",
            ObjectKind::Tree => "t_",
            ObjectKind::Changeset => "cs_",
            ObjectKind::Derived => "d_",
        }
    }

    /// Short directory name used in the blob-store key layout (SPEC §5.2).
    pub const fn store_dir(self) -> &'static str {
        match self {
            ObjectKind::Content => "c",
            ObjectKind::Chunk => "k",
            ObjectKind::Tree => "t",
            ObjectKind::Changeset => "cs",
            ObjectKind::Derived => "d",
        }
    }
}

macro_rules! define_id {
    ($(#[$meta:meta])* $name:ident, $kind:expr) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name([u8; 32]);

        impl $name {
            pub const KIND: ObjectKind = $kind;

            pub const fn from_bytes(bytes: [u8; 32]) -> Self {
                Self(bytes)
            }

            pub fn from_slice(bytes: &[u8]) -> Result<Self> {
                let arr: [u8; 32] = bytes
                    .try_into()
                    .map_err(|_| Error::InvalidId(format!("expected 32 bytes, got {}", bytes.len())))?;
                Ok(Self(arr))
            }

            /// Hash `data` in this id's domain.
            pub fn hash(data: &[u8]) -> Self {
                let mut hasher = blake3::Hasher::new_derive_key(Self::KIND.context());
                hasher.update(data);
                Self(*hasher.finalize().as_bytes())
            }

            /// Incremental hasher for streaming content.
            pub fn hasher() -> IdHasher<Self> {
                IdHasher {
                    inner: blake3::Hasher::new_derive_key(Self::KIND.context()),
                    _marker: std::marker::PhantomData,
                }
            }

            pub const fn as_bytes(&self) -> &[u8; 32] {
                &self.0
            }

            pub fn to_hex(&self) -> String {
                hex::encode(self.0)
            }

            /// Parse either `<prefix><64 hex>` or bare 64-char hex.
            pub fn parse(s: &str) -> Result<Self> {
                let hex_part = s.strip_prefix(Self::KIND.display_prefix()).unwrap_or(s);
                if hex_part.len() != 64 {
                    return Err(Error::InvalidId(format!("{s:?}: expected 64 hex chars")));
                }
                let bytes = hex::decode(hex_part).map_err(|e| Error::InvalidId(format!("{s:?}: {e}")))?;
                Self::from_slice(&bytes)
            }

            /// Short display form used by the CLI.
            pub fn short(&self) -> String {
                format!("{}{}", Self::KIND.display_prefix(), &self.to_hex()[..12])
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}{}", Self::KIND.display_prefix(), self.to_hex())
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.short())
            }
        }

        impl FromStr for $name {
            type Err = Error;
            fn from_str(s: &str) -> Result<Self> {
                Self::parse(s)
            }
        }

        impl AsRef<[u8]> for $name {
            fn as_ref(&self) -> &[u8] {
                &self.0
            }
        }

        impl serde::Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
                s.serialize_str(&self.to_string())
            }
        }

        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
                let s = <String as serde::Deserialize>::deserialize(d)?;
                Self::parse(&s).map_err(serde::de::Error::custom)
            }
        }
    };
}

define_id!(
    /// Hash of raw (uncompressed) file content (SPEC §2.2).
    ContentId,
    ObjectKind::Content
);
define_id!(
    /// Hash of a content-defined chunk of a large file.
    ChunkId,
    ObjectKind::Chunk
);
define_id!(
    /// Hash of a canonically encoded [`crate::Tree`].
    TreeId,
    ObjectKind::Tree
);
define_id!(
    /// Hash of a canonically encoded [`crate::Changeset`].
    ChangesetId,
    ObjectKind::Changeset
);
define_id!(
    /// Hash of a canonically encoded derived-data value.
    DerivedId,
    ObjectKind::Derived
);

/// Streaming hasher bound to a specific id type.
pub struct IdHasher<T> {
    inner: blake3::Hasher,
    _marker: std::marker::PhantomData<T>,
}

macro_rules! impl_hasher_finish {
    ($($t:ty),*) => {$(
        impl IdHasher<$t> {
            pub fn update(&mut self, data: &[u8]) -> &mut Self {
                self.inner.update(data);
                self
            }
            pub fn finish(&self) -> $t {
                <$t>::from_bytes(*self.inner.finalize().as_bytes())
            }
        }
    )*};
}
impl_hasher_finish!(ContentId, ChunkId, TreeId, ChangesetId, DerivedId);

impl ContentId {
    /// The id of empty content.
    pub fn empty() -> Self {
        Self::hash(&[])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domain_separation() {
        let a = ContentId::hash(b"x");
        let b = TreeId::hash(b"x");
        assert_ne!(a.as_bytes(), b.as_bytes());
    }

    #[test]
    fn display_roundtrip() {
        let id = ChangesetId::hash(b"hello");
        let s = id.to_string();
        assert!(s.starts_with("cs_"));
        assert_eq!(ChangesetId::parse(&s).unwrap(), id);
        assert_eq!(ChangesetId::parse(&id.to_hex()).unwrap(), id);
        assert!(ChangesetId::parse("cs_abc").is_err());
    }

    #[test]
    fn streaming_matches_oneshot() {
        let mut h = ContentId::hasher();
        h.update(b"hello ").update(b"world");
        assert_eq!(h.finish(), ContentId::hash(b"hello world"));
    }
}
