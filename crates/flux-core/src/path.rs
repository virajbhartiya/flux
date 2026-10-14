//! Repository paths (SPEC §3.1).
//!
//! Paths are byte strings (not necessarily UTF-8, for git fidelity) with `/`
//! as the separator. Ordering is bytewise over the full path.

use std::fmt;

use crate::error::{Error, Result};

pub const MAX_PATH_LEN: usize = 4096;
pub const MAX_COMPONENT_LEN: usize = 255;

/// A validated, non-empty repository path.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RepoPath(Vec<u8>);

/// A single validated path component. Shared, so cloning tree entries (the
/// hot path of derivation) never copies names.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PathComponent(std::sync::Arc<[u8]>);

fn invalid(path: &[u8], reason: &'static str) -> Error {
    Error::PathInvalid { path: String::from_utf8_lossy(path).into_owned(), reason }
}

fn validate_component(full: &[u8], c: &[u8]) -> Result<()> {
    if c.is_empty() {
        return Err(invalid(full, "empty component"));
    }
    if c.len() > MAX_COMPONENT_LEN {
        return Err(invalid(full, "component too long"));
    }
    if c == b"." || c == b".." {
        return Err(invalid(full, "'.' and '..' components are not allowed"));
    }
    if c == b".flux" {
        return Err(invalid(full, "'.flux' is reserved"));
    }
    if c.contains(&0) {
        return Err(invalid(full, "NUL byte"));
    }
    if c.contains(&b'/') {
        return Err(invalid(full, "'/' in component"));
    }
    Ok(())
}

impl RepoPath {
    pub fn new(bytes: impl Into<Vec<u8>>) -> Result<Self> {
        let bytes = bytes.into();
        if bytes.is_empty() {
            return Err(invalid(&bytes, "empty path"));
        }
        if bytes.len() > MAX_PATH_LEN {
            return Err(invalid(&bytes, "path too long"));
        }
        for c in bytes.split(|b| *b == b'/') {
            validate_component(&bytes, c)?;
        }
        Ok(Self(bytes))
    }

    pub fn from_components<'a>(parts: impl IntoIterator<Item = &'a [u8]>) -> Result<Self> {
        let mut out = Vec::new();
        for (i, p) in parts.into_iter().enumerate() {
            if i > 0 {
                out.push(b'/');
            }
            out.extend_from_slice(p);
        }
        Self::new(out)
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }

    pub fn components(&self) -> impl Iterator<Item = &[u8]> {
        self.0.split(|b| *b == b'/')
    }

    /// Parent directory path, or `None` for a top-level entry.
    pub fn parent(&self) -> Option<RepoPath> {
        let idx = self.0.iter().rposition(|b| *b == b'/')?;
        Some(RepoPath(self.0[..idx].to_vec()))
    }

    pub fn basename(&self) -> &[u8] {
        match self.0.iter().rposition(|b| *b == b'/') {
            Some(idx) => &self.0[idx + 1..],
            None => &self.0,
        }
    }

    /// Append a component.
    pub fn join(&self, component: &[u8]) -> Result<RepoPath> {
        let mut v = self.0.clone();
        v.push(b'/');
        v.extend_from_slice(component);
        RepoPath::new(v)
    }

    /// True if `self` equals `prefix` or lives underneath it.
    pub fn starts_with(&self, prefix: &RepoPath) -> bool {
        self.0.starts_with(&prefix.0)
            && (self.0.len() == prefix.0.len() || self.0[prefix.0.len()] == b'/')
    }

    /// Lossy UTF-8 rendering for display.
    pub fn display(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.0)
    }
}

impl PathComponent {
    pub fn new(bytes: impl Into<Vec<u8>>) -> Result<Self> {
        let bytes = bytes.into();
        validate_component(&bytes, &bytes)?;
        Ok(Self(bytes.into()))
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for RepoPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.display())
    }
}

impl fmt::Display for RepoPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.display())
    }
}

impl fmt::Debug for PathComponent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", String::from_utf8_lossy(&self.0))
    }
}

impl TryFrom<&str> for RepoPath {
    type Error = Error;
    fn try_from(s: &str) -> Result<Self> {
        RepoPath::new(s.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_paths() {
        for p in ["a", "a/b", "a/b/c.txt", "with space/x", "unicodé/ß"] {
            RepoPath::new(p).unwrap();
        }
        RepoPath::new(vec![0xff, 0xfe, b'/', b'x']).unwrap();
    }

    #[test]
    fn invalid_paths() {
        for p in ["", "/a", "a/", "a//b", ".", "a/./b", "a/../b", ".flux/x", "a/.flux", "a\0b"] {
            assert!(RepoPath::new(p).is_err(), "{p:?} should be invalid");
        }
        assert!(RepoPath::new("x".repeat(256)).is_err());
        assert!(RepoPath::new(["a"; 3000].join("/")).is_err());
    }

    #[test]
    fn parent_and_basename() {
        let p = RepoPath::try_from("a/b/c").unwrap();
        assert_eq!(p.basename(), b"c");
        assert_eq!(p.parent().unwrap().as_bytes(), b"a/b");
        assert!(RepoPath::try_from("a").unwrap().parent().is_none());
    }

    #[test]
    fn starts_with_respects_components() {
        let p = RepoPath::try_from("foo/bar").unwrap();
        assert!(p.starts_with(&RepoPath::try_from("foo").unwrap()));
        assert!(p.starts_with(&RepoPath::try_from("foo/bar").unwrap()));
        assert!(!p.starts_with(&RepoPath::try_from("fo").unwrap()));
    }
}
