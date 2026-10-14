//! Trees: the materialized file set at a changeset (SPEC §3.4).
//!
//! Trees are derived data (`tree_v1`), not part of a changeset's identity.
//!
//! Canonical encoding:
//! ```text
//! [ 1, [[name: bytes, kind, id: bytes32, size: uint, descendant_files: uint], ...] ]
//! kind = [0] regular | [1] executable | [2] symlink | [3, git_sha1] | [4] dir
//! ```
//! Entries are sorted bytewise by name and names are unique. For gitlinks the
//! id field is all zeros (the submodule commit lives in the kind).

use crate::cbor::{Decoder, Encoder};
use crate::changeset::{FileKind, decode_kind, encode_kind};
use crate::error::{Error, Result};
use crate::id::{ContentId, TreeId};
use crate::path::PathComponent;

const FORMAT_VERSION: u64 = 1;
pub const MAX_TREE_ENTRIES: usize = 65_536;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EntryKind {
    File(FileKind),
    Dir,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TreeEntry {
    pub name: PathComponent,
    pub kind: EntryKind,
    /// `ContentId` bytes for files, `TreeId` bytes for directories.
    pub id: [u8; 32],
    /// File size, or recursive total for directories.
    pub size: u64,
    /// 1 for files, recursive file count for directories.
    pub descendant_files: u64,
}

impl TreeEntry {
    pub fn file(name: PathComponent, kind: FileKind, content_id: ContentId, size: u64) -> Self {
        let id = match kind {
            FileKind::GitLink(_) => [0u8; 32],
            _ => *content_id.as_bytes(),
        };
        Self { name, kind: EntryKind::File(kind), id, size, descendant_files: 1 }
    }

    pub fn dir(name: PathComponent, tree_id: TreeId, size: u64, descendant_files: u64) -> Self {
        Self { name, kind: EntryKind::Dir, id: *tree_id.as_bytes(), size, descendant_files }
    }

    pub fn is_dir(&self) -> bool {
        matches!(self.kind, EntryKind::Dir)
    }

    pub fn tree_id(&self) -> Option<TreeId> {
        self.is_dir().then(|| TreeId::from_bytes(self.id))
    }

    pub fn content_id(&self) -> Option<ContentId> {
        match self.kind {
            EntryKind::File(FileKind::GitLink(_)) | EntryKind::Dir => None,
            EntryKind::File(_) => Some(ContentId::from_bytes(self.id)),
        }
    }

    pub fn file_kind(&self) -> Option<FileKind> {
        match self.kind {
            EntryKind::File(k) => Some(k),
            EntryKind::Dir => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Tree {
    entries: Vec<TreeEntry>,
}

impl Tree {
    /// Build a tree, sorting entries and rejecting duplicate names.
    pub fn new(mut entries: Vec<TreeEntry>) -> Result<Self> {
        if entries.len() > MAX_TREE_ENTRIES {
            return Err(Error::InvalidObject(format!("tree has {} entries (max {MAX_TREE_ENTRIES})", entries.len())));
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        for w in entries.windows(2) {
            if w[0].name == w[1].name {
                return Err(Error::InvalidObject(format!("duplicate tree entry {:?}", w[0].name)));
            }
        }
        Ok(Self { entries })
    }

    /// Build a tree from entries already sorted by name (checked in O(n)).
    pub fn from_sorted(entries: Vec<TreeEntry>) -> Result<Self> {
        if entries.len() > MAX_TREE_ENTRIES {
            return Err(Error::InvalidObject(format!("tree has {} entries (max {MAX_TREE_ENTRIES})", entries.len())));
        }
        for w in entries.windows(2) {
            if w[0].name >= w[1].name {
                return Err(Error::InvalidObject(format!("tree entries unsorted or duplicate at {:?}", w[1].name)));
            }
        }
        Ok(Self { entries })
    }

    pub fn empty() -> Self {
        Self::default()
    }

    pub fn entries(&self) -> &[TreeEntry] {
        &self.entries
    }

    pub fn into_entries(self) -> Vec<TreeEntry> {
        self.entries
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn get(&self, name: &[u8]) -> Option<&TreeEntry> {
        self.entries.binary_search_by(|e| e.name.as_bytes().cmp(name)).ok().map(|i| &self.entries[i])
    }

    /// Recursive total size of everything in this tree.
    pub fn total_size(&self) -> u64 {
        self.entries.iter().map(|e| e.size).sum()
    }

    pub fn total_files(&self) -> u64 {
        self.entries.iter().map(|e| e.descendant_files).sum()
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut e = Encoder::new();
        e.array(2).uint(FORMAT_VERSION).array(self.entries.len());
        for entry in &self.entries {
            e.array(5).bytes(entry.name.as_bytes());
            match entry.kind {
                EntryKind::File(k) => encode_kind(&mut e, &k),
                EntryKind::Dir => {
                    e.array(1).uint(4);
                }
            }
            e.bytes(&entry.id).uint(entry.size).uint(entry.descendant_files);
        }
        e.into_bytes()
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let mut d = Decoder::new(data);
        d.array_exact(2)?;
        let version = d.uint()?;
        if version != FORMAT_VERSION {
            return Err(Error::Decode(format!("unsupported tree version {version}")));
        }
        let n = d.array()?;
        if n > MAX_TREE_ENTRIES {
            return Err(Error::InvalidObject("too many tree entries".into()));
        }
        let mut entries: Vec<TreeEntry> = Vec::with_capacity(n);
        for _ in 0..n {
            d.array_exact(5)?;
            let name = PathComponent::new(d.bytes()?)?;
            if let Some(prev) = entries.last()
                && prev.name >= name
            {
                return Err(Error::NonCanonical);
            }
            // Peek the kind tag: 4 means directory, otherwise a file kind.
            let kind = decode_entry_kind(&mut d)?;
            let id = d.bytes_fixed::<32>()?;
            let size = d.uint()?;
            let descendant_files = d.uint()?;
            d.end();
            if let EntryKind::File(FileKind::GitLink(_)) = kind
                && id != [0u8; 32]
            {
                return Err(Error::NonCanonical);
            }
            if !matches!(kind, EntryKind::Dir) && descendant_files != 1 {
                return Err(Error::InvalidObject("file entry must have descendant_files = 1".into()));
            }
            entries.push(TreeEntry { name, kind, id, size, descendant_files });
        }
        d.end();
        d.end();
        d.finish()?;
        Ok(Self { entries })
    }

    pub fn id(&self) -> TreeId {
        TreeId::hash(&self.encode())
    }

    pub fn decode_verified(data: &[u8], expected: &TreeId) -> Result<Self> {
        let actual = TreeId::hash(data);
        if &actual != expected {
            return Err(Error::InvalidObject(format!("tree hash mismatch: {actual} != {expected}")));
        }
        Self::decode(data)
    }
}

fn decode_entry_kind(d: &mut Decoder<'_>) -> Result<EntryKind> {
    // Dir is encoded as [4]; file kinds share the changeset encoding. We decode
    // the dir case by hand and delegate otherwise using a cloned decoder state.
    let saved = d.clone_state();
    let n = d.array()?;
    let tag = d.uint()?;
    if tag == 4 {
        if n != 1 {
            return Err(Error::Decode("bad dir kind".into()));
        }
        d.end();
        return Ok(EntryKind::Dir);
    }
    d.restore_state(saved);
    Ok(EntryKind::File(decode_kind(d)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn comp(s: &str) -> PathComponent {
        PathComponent::new(s).unwrap()
    }

    #[test]
    fn roundtrip_and_sorting() {
        let sub = Tree::new(vec![TreeEntry::file(comp("x"), FileKind::Regular, ContentId::hash(b"x"), 1)]).unwrap();
        let t = Tree::new(vec![
            TreeEntry::file(comp("b.txt"), FileKind::Executable, ContentId::hash(b"b"), 1),
            TreeEntry::dir(comp("a"), sub.id(), sub.total_size(), sub.total_files()),
            TreeEntry::file(comp("sub"), FileKind::GitLink([7; 20]), ContentId::empty(), 0),
            TreeEntry::file(comp("link"), FileKind::Symlink, ContentId::hash(b"target"), 6),
        ])
        .unwrap();
        assert_eq!(t.entries()[0].name, comp("a"));
        let bytes = t.encode();
        let back = Tree::decode(&bytes).unwrap();
        assert_eq!(back, t);
        assert_eq!(back.encode(), bytes);
        assert_eq!(t.total_files(), 4);
        assert!(t.get(b"link").is_some());
        assert!(t.get(b"nope").is_none());
    }

    #[test]
    fn rejects_duplicates() {
        let e = TreeEntry::file(comp("a"), FileKind::Regular, ContentId::empty(), 0);
        assert!(Tree::new(vec![e.clone(), e]).is_err());
    }

    #[test]
    fn empty_tree_is_stable() {
        assert_eq!(Tree::empty().id(), Tree::decode(&Tree::empty().encode()).unwrap().id());
    }
}
