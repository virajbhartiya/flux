//! Changesets: the unit of truth (SPEC §3.3).
//!
//! A changeset records the delta against its parents, never a tree pointer.
//!
//! Canonical encoding (CBOR array, positional):
//! ```text
//! [ 1,                                   ; format version
//!   [parent_id: bytes32, ...],
//!   author: bytes, author_time: [unix_ms, tz_offset_min],
//!   committer: bytes | null, committer_time: [..] | null,
//!   message: bytes,
//!   [[path: bytes, change], ...],        ; sorted bytewise by path
//!   [[key: text, value: bytes], ...] ]   ; sorted bytewise by key
//! change   = [0]                                          ; deletion
//!          | [1, content_id, size, kind, copy_from | null]
//!          | [2]                                          ; retain (merges only)
//! kind     = [0] regular | [1] executable | [2] symlink | [3, git_sha1: bytes20]
//! copy_from = [path: bytes, parent_index: uint]
//! ```

use std::collections::BTreeMap;

use crate::cbor::{Decoder, Encoder};
use crate::error::{Error, Result};
use crate::id::{ChangesetId, ContentId};
use crate::path::RepoPath;

const FORMAT_VERSION: u64 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Timestamp {
    pub unix_ms: i64,
    pub tz_offset_min: i16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileKind {
    Regular,
    Executable,
    Symlink,
    /// A git submodule pointer; content is not stored in flux.
    GitLink([u8; 20]),
    /// A plain file recorded with a non-canonical git mode (`0o100664` in
    /// early git history, say). Kept so imported trees hash to their
    /// original SHA-1s; otherwise it behaves as `Regular`, or `Executable`
    /// when any execute bit is set.
    GitMode(u32),
}

impl FileKind {
    /// The kind for a raw git file mode (`0o100xxx`), keeping odd modes.
    pub fn from_git_file_mode(mode: u32) -> Self {
        match mode {
            0o100644 => FileKind::Regular,
            0o100755 => FileKind::Executable,
            m => FileKind::GitMode(m),
        }
    }

    pub fn is_executable(self) -> bool {
        match self {
            FileKind::Executable => true,
            FileKind::GitMode(m) => m & 0o111 != 0,
            _ => false,
        }
    }

    /// Regular or executable file content (not a symlink or gitlink).
    pub fn is_plain(self) -> bool {
        matches!(self, FileKind::Regular | FileKind::Executable | FileKind::GitMode(_))
    }

    /// The canonical kind this one behaves as.
    pub fn normalized(self) -> Self {
        match self {
            FileKind::GitMode(_) if self.is_executable() => FileKind::Executable,
            FileKind::GitMode(_) => FileKind::Regular,
            k => k,
        }
    }

    fn valid_git_mode(m: u32) -> bool {
        m & !0o777 == 0o100000 && m != 0o100644 && m != 0o100755
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum FileChange {
    Change {
        content_id: ContentId,
        size: u64,
        kind: FileKind,
        copy_from: Option<(RepoPath, u32)>,
    },
    Deletion,
    /// Merges only: the parents disagree at this path and the resolution is
    /// `parents[0]`'s value (which may be absence). Listing it satisfies the
    /// merge completeness rule without repeating the content.
    Retain,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Changeset {
    pub parents: Vec<ChangesetId>,
    pub author: Vec<u8>,
    pub author_time: Timestamp,
    pub committer: Option<Vec<u8>>,
    pub committer_time: Option<Timestamp>,
    pub message: Vec<u8>,
    pub file_changes: BTreeMap<RepoPath, FileChange>,
    pub extra: BTreeMap<String, Vec<u8>>,
}

impl Changeset {
    /// Structural validation that does not need access to parents (SPEC §3.3).
    pub fn validate(&self) -> Result<()> {
        if self.parents.len() < 2 && self.file_changes.values().any(|c| matches!(c, FileChange::Retain)) {
            return Err(Error::InvalidObject("retain entries are only valid in merges".into()));
        }
        // No duplicate parents.
        for (i, p) in self.parents.iter().enumerate() {
            if self.parents[..i].contains(p) {
                return Err(Error::InvalidObject(format!("duplicate parent {p}")));
            }
        }
        for (path, change) in &self.file_changes {
            if let FileChange::Change { copy_from, kind, .. } = change {
                if let Some((_, idx)) = copy_from
                    && *idx as usize >= self.parents.len()
                {
                    return Err(Error::InvalidObject(format!("copy_from parent index {idx} out of range for {path}")));
                }
                if matches!(kind, FileKind::GitLink(_)) && copy_from.is_some() {
                    return Err(Error::InvalidObject("gitlink cannot have copy_from".into()));
                }
                if let FileKind::GitMode(m) = kind
                    && !FileKind::valid_git_mode(*m)
                {
                    return Err(Error::InvalidObject(format!("invalid git mode {m:o} for {path}")));
                }
            }
        }
        // A changeset may not create both `a` and `a/b` as files. Deletions
        // underneath a created file are allowed: they spell out the implicit
        // dir→file replacement (git emits them that way). A sorted adjacency
        // check is not enough because bytes like '-' sort before '/'.
        let files: std::collections::HashSet<&[u8]> =
            self.file_changes.iter().filter(|(_, c)| matches!(c, FileChange::Change { .. })).map(|(p, _)| p.as_bytes()).collect();
        let mut change_ancestors: std::collections::HashSet<&[u8]> = std::collections::HashSet::new();
        for (path, change) in &self.file_changes {
            if matches!(change, FileChange::Deletion) {
                continue;
            }
            // A retained path keeps parents[0]'s value, which nothing else in
            // this changeset may contradict (via an ancestor or descendant).
            let retain = matches!(change, FileChange::Retain);
            let bytes = path.as_bytes();
            let mut cut = bytes.len();
            while let Some(i) = bytes[..cut].iter().rposition(|b| *b == b'/') {
                let a = &bytes[..i];
                if files.contains(a) {
                    return Err(Error::PathConflict(String::from_utf8_lossy(a).into_owned()));
                }
                if !retain {
                    change_ancestors.insert(a);
                }
                cut = i;
            }
        }
        for (path, change) in &self.file_changes {
            if matches!(change, FileChange::Retain) && change_ancestors.contains(path.as_bytes()) {
                return Err(Error::PathConflict(path.to_string()));
            }
        }
        Ok(())
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut e = Encoder::new();
        e.array(9);
        e.uint(FORMAT_VERSION);
        e.array(self.parents.len());
        for p in &self.parents {
            e.bytes(p.as_bytes());
        }
        e.bytes(&self.author);
        encode_ts(&mut e, &self.author_time);
        match &self.committer {
            Some(c) => e.bytes(c),
            None => e.null(),
        };
        match &self.committer_time {
            Some(t) => encode_ts(&mut e, t),
            None => {
                e.null();
            }
        }
        e.bytes(&self.message);
        e.array(self.file_changes.len());
        for (path, change) in &self.file_changes {
            e.array(2);
            e.bytes(path.as_bytes());
            encode_change(&mut e, change);
        }
        e.array(self.extra.len());
        for (k, v) in &self.extra {
            e.array(2);
            e.text(k);
            e.bytes(v);
        }
        e.into_bytes()
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let mut d = Decoder::new(data);
        d.array_exact(9)?;
        let version = d.uint()?;
        if version != FORMAT_VERSION {
            return Err(Error::Decode(format!("unsupported changeset version {version}")));
        }
        let n = d.array()?;
        let mut parents = Vec::with_capacity(n);
        for _ in 0..n {
            parents.push(ChangesetId::from_bytes(d.bytes_fixed()?));
        }
        d.end();
        let author = d.bytes()?.to_vec();
        let author_time = decode_ts(&mut d)?;
        let committer = if d.null()? { None } else { Some(d.bytes()?.to_vec()) };
        let committer_time = if d.null()? { None } else { Some(decode_ts(&mut d)?) };
        let message = d.bytes()?.to_vec();
        let n = d.array()?;
        let mut file_changes = BTreeMap::new();
        let mut last: Option<RepoPath> = None;
        for _ in 0..n {
            d.array_exact(2)?;
            let path = RepoPath::new(d.bytes()?)?;
            if let Some(l) = &last
                && l >= &path
            {
                return Err(Error::NonCanonical);
            }
            let change = decode_change(&mut d)?;
            d.end();
            last = Some(path.clone());
            file_changes.insert(path, change);
        }
        d.end();
        let n = d.array()?;
        let mut extra = BTreeMap::new();
        let mut last_key: Option<String> = None;
        for _ in 0..n {
            d.array_exact(2)?;
            let k = d.text()?.to_owned();
            if let Some(l) = &last_key
                && l.as_bytes() >= k.as_bytes()
            {
                return Err(Error::NonCanonical);
            }
            let v = d.bytes()?.to_vec();
            d.end();
            last_key = Some(k.clone());
            extra.insert(k, v);
        }
        d.end();
        d.end();
        d.finish()?;
        Ok(Self { parents, author, author_time, committer, committer_time, message, file_changes, extra })
    }

    pub fn id(&self) -> ChangesetId {
        ChangesetId::hash(&self.encode())
    }

    /// Decode and return the id, verifying it matches `expected`.
    pub fn decode_verified(data: &[u8], expected: &ChangesetId) -> Result<Self> {
        let actual = ChangesetId::hash(data);
        if &actual != expected {
            return Err(Error::InvalidObject(format!("changeset hash mismatch: {actual} != {expected}")));
        }
        Self::decode(data)
    }
}

fn encode_ts(e: &mut Encoder, t: &Timestamp) {
    e.array(2);
    e.int(t.unix_ms);
    e.int(t.tz_offset_min as i64);
}

fn decode_ts(d: &mut Decoder<'_>) -> Result<Timestamp> {
    d.array_exact(2)?;
    let unix_ms = d.int()?;
    let tz = d.int()?;
    d.end();
    let tz_offset_min = i16::try_from(tz).map_err(|_| Error::Decode("tz offset out of range".into()))?;
    Ok(Timestamp { unix_ms, tz_offset_min })
}

pub(crate) fn encode_kind(e: &mut Encoder, kind: &FileKind) {
    match kind {
        FileKind::Regular => e.array(1).uint(0),
        FileKind::Executable => e.array(1).uint(1),
        FileKind::Symlink => e.array(1).uint(2),
        FileKind::GitLink(sha) => e.array(2).uint(3).bytes(sha),
        // Tag 4 is a directory in tree entries (tree.rs).
        FileKind::GitMode(m) => e.array(2).uint(5).uint(*m as u64),
    };
}

pub(crate) fn decode_kind(d: &mut Decoder<'_>) -> Result<FileKind> {
    let n = d.array()?;
    let tag = d.uint()?;
    let kind = match (tag, n) {
        (0, 1) => FileKind::Regular,
        (1, 1) => FileKind::Executable,
        (2, 1) => FileKind::Symlink,
        (3, 2) => FileKind::GitLink(d.bytes_fixed()?),
        (5, 2) => {
            let m = u32::try_from(d.uint()?).map_err(|_| Error::Decode("git mode out of range".into()))?;
            if !FileKind::valid_git_mode(m) {
                return Err(Error::NonCanonical);
            }
            FileKind::GitMode(m)
        }
        _ => return Err(Error::Decode(format!("bad file kind tag {tag}/{n}"))),
    };
    d.end();
    Ok(kind)
}

fn encode_change(e: &mut Encoder, change: &FileChange) {
    match change {
        FileChange::Deletion => {
            e.array(1).uint(0);
        }
        FileChange::Retain => {
            e.array(1).uint(2);
        }
        FileChange::Change { content_id, size, kind, copy_from } => {
            e.array(5).uint(1).bytes(content_id.as_bytes()).uint(*size);
            encode_kind(e, kind);
            match copy_from {
                Some((p, idx)) => e.array(2).bytes(p.as_bytes()).uint(*idx as u64),
                None => e.null(),
            };
        }
    }
}

fn decode_change(d: &mut Decoder<'_>) -> Result<FileChange> {
    let n = d.array()?;
    let tag = d.uint()?;
    let change = match (tag, n) {
        (0, 1) => FileChange::Deletion,
        (2, 1) => FileChange::Retain,
        (1, 5) => {
            let content_id = ContentId::from_bytes(d.bytes_fixed()?);
            let size = d.uint()?;
            let kind = decode_kind(d)?;
            let copy_from = if d.null()? {
                None
            } else {
                d.array_exact(2)?;
                let p = RepoPath::new(d.bytes()?)?;
                let idx = u32::try_from(d.uint()?).map_err(|_| Error::Decode("parent index overflow".into()))?;
                d.end();
                Some((p, idx))
            };
            FileChange::Change { content_id, size, kind, copy_from }
        }
        _ => return Err(Error::Decode(format!("bad file change tag {tag}/{n}"))),
    };
    d.end();
    Ok(change)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn sample() -> Changeset {
        let mut file_changes = BTreeMap::new();
        file_changes.insert(
            RepoPath::try_from("src/main.rs").unwrap(),
            FileChange::Change {
                content_id: ContentId::hash(b"fn main() {}\n"),
                size: 13,
                kind: FileKind::Regular,
                copy_from: None,
            },
        );
        file_changes.insert(RepoPath::try_from("old.txt").unwrap(), FileChange::Deletion);
        file_changes.insert(
            RepoPath::try_from("bin/run").unwrap(),
            FileChange::Change {
                content_id: ContentId::hash(b"#!/bin/sh\n"),
                size: 10,
                kind: FileKind::Executable,
                copy_from: Some((RepoPath::try_from("scripts/run").unwrap(), 0)),
            },
        );
        let mut extra = BTreeMap::new();
        extra.insert("flux.review.id".to_string(), b"42".to_vec());
        Changeset {
            parents: vec![ChangesetId::hash(b"parent")],
            author: b"Ada <ada@example.com>".to_vec(),
            author_time: Timestamp { unix_ms: 1_760_000_000_000, tz_offset_min: -420 },
            committer: None,
            committer_time: None,
            message: b"Add main\n".to_vec(),
            file_changes,
            extra,
        }
    }

    #[test]
    fn roundtrip() {
        let cs = sample();
        cs.validate().unwrap();
        let bytes = cs.encode();
        let back = Changeset::decode(&bytes).unwrap();
        assert_eq!(back, cs);
        assert_eq!(back.encode(), bytes);
        assert_eq!(Changeset::decode_verified(&bytes, &cs.id()).unwrap(), cs);
    }

    #[test]
    fn rejects_file_dir_conflict() {
        let mut cs = sample();
        cs.file_changes.insert(
            RepoPath::try_from("src/main.rs/inner").unwrap(),
            FileChange::Change { content_id: ContentId::empty(), size: 0, kind: FileKind::Regular, copy_from: None },
        );
        assert!(matches!(cs.validate(), Err(Error::PathConflict(_))));
    }

    #[test]
    fn conflict_detected_across_non_adjacent_sort() {
        // "a" < "a-b" < "a/c" bytewise: "a" and "a/c" are not adjacent.
        let file = FileChange::Change { content_id: ContentId::empty(), size: 0, kind: FileKind::Regular, copy_from: None };
        let mut cs = sample();
        cs.file_changes.clear();
        for p in ["a", "a-b", "a/c"] {
            cs.file_changes.insert(RepoPath::try_from(p).unwrap(), file.clone());
        }
        assert!(matches!(cs.validate(), Err(Error::PathConflict(_))));
    }

    #[test]
    fn deletion_under_file_is_conflict_free_for_dirs() {
        // Deleting `a/b` while creating file `a` is a valid dir→file replacement.
        let mut cs = sample();
        cs.file_changes.clear();
        cs.file_changes.insert(RepoPath::try_from("a/b").unwrap(), FileChange::Deletion);
        cs.file_changes.insert(
            RepoPath::try_from("a").unwrap(),
            FileChange::Change { content_id: ContentId::empty(), size: 0, kind: FileKind::Regular, copy_from: None },
        );
        cs.validate().unwrap();
    }

    #[test]
    fn retain_rules() {
        let mut cs = sample();
        cs.file_changes.insert(RepoPath::try_from("kept").unwrap(), FileChange::Retain);
        assert!(cs.validate().is_err(), "retain needs a merge");
        cs.parents.push(ChangesetId::hash(b"other"));
        cs.validate().unwrap();
        let back = Changeset::decode(&cs.encode()).unwrap();
        assert_eq!(back, cs);
        // A change underneath a retained path contradicts it.
        cs.file_changes.insert(
            RepoPath::try_from("kept/inner").unwrap(),
            FileChange::Change { content_id: ContentId::empty(), size: 0, kind: FileKind::Regular, copy_from: None },
        );
        assert!(matches!(cs.validate(), Err(Error::PathConflict(_))));
    }

    #[test]
    fn rejects_trailing_bytes() {
        let mut bytes = sample().encode();
        bytes.push(0);
        assert!(Changeset::decode(&bytes).is_err());
    }

    #[test]
    fn legacy_git_modes_roundtrip() {
        let mut cs = sample();
        cs.file_changes.insert(
            RepoPath::try_from("legacy").unwrap(),
            FileChange::Change { content_id: ContentId::empty(), size: 0, kind: FileKind::GitMode(0o100664), copy_from: None },
        );
        cs.validate().unwrap();
        assert_eq!(Changeset::decode(&cs.encode()).unwrap(), cs);
        assert_eq!(FileKind::from_git_file_mode(0o100644), FileKind::Regular);
        assert!(FileKind::GitMode(0o100775).is_executable());
        assert_eq!(FileKind::GitMode(0o100664).normalized(), FileKind::Regular);
        cs.file_changes.insert(
            RepoPath::try_from("bad").unwrap(),
            FileChange::Change { content_id: ContentId::empty(), size: 0, kind: FileKind::GitMode(0o100644), copy_from: None },
        );
        assert!(cs.validate().is_err(), "canonical modes must use Regular/Executable");
    }
}
