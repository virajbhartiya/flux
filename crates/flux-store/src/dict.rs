//! Trained zstd dictionaries (codec 2).
//!
//! Small objects (tree nodes, changesets, short source files) compress
//! poorly on their own because zstd has nothing to learn from; a dictionary
//! trained on a sample of a repository's objects fixes that. Dictionaries
//! are immutable and numbered; an object names its dictionary in the
//! header's `dict_id` field. Each kind uses the newest dictionary trained
//! for it, and older dictionaries stay readable forever.
//!
//! File format (`dicts/{id}.zdict` in the segment directory, or
//! `{namespace}/dicts/{id}` in an object store):
//! `magic "FLXD" | version u8 (1) | kind u8 | id u16 BE | blake3 [32] | dict`.
//! The checksum covers the dictionary bytes. Every object decoded with a
//! dictionary is still verified against its id, so a bad dictionary can
//! never yield wrong data, only an error.

use std::collections::HashMap;
use std::sync::Arc;

use flux_core::ObjectKind;

use crate::meta::{StoreError, StoreResult};

const MAGIC: &[u8; 4] = b"FLXD";
const HEADER: usize = 4 + 1 + 1 + 2 + 32;
/// Objects up to this size are compressed with the kind's dictionary.
pub const DICT_MAX_OBJECT: usize = 64 << 10;
/// Default trained dictionary size (zstd's recommendation).
pub const DICT_SIZE: usize = 112 << 10;

pub struct Dict {
    pub id: u16,
    pub kind: ObjectKind,
    pub raw: Vec<u8>,
    pub enc: zstd::dict::EncoderDictionary<'static>,
    pub dec: zstd::dict::DecoderDictionary<'static>,
}

impl std::fmt::Debug for Dict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dict").field("id", &self.id).field("kind", &self.kind).field("bytes", &self.raw.len()).finish()
    }
}

pub(crate) fn kind_code(kind: ObjectKind) -> u8 {
    match kind {
        ObjectKind::Content => 0,
        ObjectKind::Chunk => 1,
        ObjectKind::Tree => 2,
        ObjectKind::Changeset => 3,
        ObjectKind::Derived => 4,
    }
}

fn kind_from_code(b: u8) -> StoreResult<ObjectKind> {
    Ok(match b {
        0 => ObjectKind::Content,
        1 => ObjectKind::Chunk,
        2 => ObjectKind::Tree,
        3 => ObjectKind::Changeset,
        4 => ObjectKind::Derived,
        k => return Err(StoreError::Corrupt(format!("dictionary for unknown kind {k}"))),
    })
}

impl Dict {
    pub fn new(id: u16, kind: ObjectKind, raw: Vec<u8>, level: i32) -> Self {
        let enc = zstd::dict::EncoderDictionary::copy(&raw, level);
        let dec = zstd::dict::DecoderDictionary::copy(&raw);
        Self { id, kind, raw, enc, dec }
    }

    pub fn encode_file(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER + self.raw.len());
        out.extend_from_slice(MAGIC);
        out.push(1);
        out.push(kind_code(self.kind));
        out.extend_from_slice(&self.id.to_be_bytes());
        out.extend_from_slice(blake3::hash(&self.raw).as_bytes());
        out.extend_from_slice(&self.raw);
        out
    }

    pub fn decode_file(data: &[u8], level: i32) -> StoreResult<Self> {
        if data.len() < HEADER || &data[..4] != MAGIC || data[4] != 1 {
            return Err(StoreError::Corrupt("bad dictionary header".into()));
        }
        let kind = kind_from_code(data[5])?;
        let id = u16::from_be_bytes([data[6], data[7]]);
        let raw = &data[HEADER..];
        if blake3::hash(raw).as_bytes() != &data[8..40] {
            return Err(StoreError::Corrupt(format!("dictionary {id} checksum mismatch")));
        }
        Ok(Self::new(id, kind, raw.to_vec(), level))
    }
}

/// Every known dictionary, plus the active (newest) one per kind.
#[derive(Debug, Default, Clone)]
pub struct DictSet {
    pub by_id: HashMap<u16, Arc<Dict>>,
}

impl DictSet {
    pub fn insert(&mut self, d: Dict) {
        self.by_id.insert(d.id, Arc::new(d));
    }

    pub fn get(&self, id: u16) -> Option<&Arc<Dict>> {
        self.by_id.get(&id)
    }

    pub fn active(&self, kind: ObjectKind) -> Option<&Arc<Dict>> {
        self.by_id.values().filter(|d| d.kind == kind).max_by_key(|d| d.id)
    }

    pub fn next_id(&self) -> u16 {
        self.by_id.keys().max().map_or(1, |m| m + 1)
    }
}

/// Train a dictionary from sample objects.
pub fn train(samples: &[Vec<u8>], size: usize) -> StoreResult<Vec<u8>> {
    if samples.len() < 16 {
        return Err(StoreError::Backend(anyhow::anyhow!(
            "need at least 16 samples to train a dictionary, got {}",
            samples.len()
        )));
    }
    zstd::dict::from_samples(samples, size).map_err(|e| StoreError::Backend(anyhow::anyhow!("training failed: {e}")))
}
