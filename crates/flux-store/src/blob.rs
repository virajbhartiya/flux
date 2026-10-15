//! Content-addressed blob store (SPEC §5.2).
//!
//! Object format: a 16-byte header followed by the payload.
//! `magic "FLX1" | codec u8 | kind u8 | dict_id u16 BE | uncompressed_len u64 BE`.
//!
//! Codecs:
//! * `0` raw, `1` zstd;
//! * `2` zstd with a trained dictionary (`dict_id` names it; see [`crate::dict`]);
//! * `3` zstd delta: payload = `base_id [32] | zstd frame` compressed with the
//!   base object's decoded bytes as a raw-content dictionary. `dict_id` holds
//!   the chain depth; chains are capped (see [`max_depth`]) so a cold read
//!   costs a bounded number of decodes, and every decoded link is cached;
//! * `4` chunk manifest (content > 4 MiB): payload = `count u32 BE` then
//!   `chunk_id [32] | len u32 BE` per chunk (SPEC §3.2).
//!
//! Backends: a segment store for `file://` (see [`crate::segment`]) and any
//! `object_store` backend (S3-compatible, in-memory) keyed
//! `{namespace}/{type}/{id[0..2]}/{id[2..4]}/{id_hex}`.
//!
//! Every object read from a backend is verified against its id before it is
//! returned or cached.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use bytes::Bytes;
use flux_core::{ChunkId, ContentId, ObjectKind, Tree, TreeId};
use futures::StreamExt;
use object_store::path::Path as ObjPath;
use object_store::{ObjectStore, PutPayload};
use rayon::prelude::*;
use sha1::Digest as _;

use crate::dict::{DICT_MAX_OBJECT, Dict, DictSet};
use crate::meta::{ContentMeta, StoreError, StoreResult};
use crate::segment::SegmentStore;

const MAGIC: &[u8; 4] = b"FLX1";
const HEADER_LEN: usize = 16;
const ZSTD_LEVEL: i32 = 3;
/// Level for deltas. (Level 1's fast strategy halves table-building cost but
/// finds far fewer matches in the base: storage doubled on git.git.)
const ZSTD_DELTA_LEVEL: i32 = 3;
/// Don't bother compressing tiny objects.
const MIN_COMPRESS: usize = 64;
/// Objects larger than this are never delta-encoded.
const MAX_DELTA_BYTES: usize = 2 << 20;
/// Content larger than this is split into chunks (SPEC §3.2).
pub const CHUNK_THRESHOLD: usize = 4 << 20;
const CHUNK_MIN: u32 = 512 << 10;
const CHUNK_AVG: u32 = 1 << 20;
const CHUNK_MAX: u32 = 4 << 20;
/// Decoded objects up to this size are kept in the in-process cache.
const CACHE_OBJECT_MAX: usize = 4 << 20;
const ZSTD_DICT_MAGIC: [u8; 4] = [0x37, 0xA4, 0x30, 0xEC];

/// Longest delta chain per kind. Trees and derived index nodes are mostly
/// 32-byte ids, which compress poorly on their own, change by a few entries
/// per version and are read through caches, so they get git's default depth
/// (a whole copy every 51 versions instead of every 17 halved their
/// storage on git.git). File contents keep shorter chains for cold reads.
fn max_depth(kind: ObjectKind) -> u16 {
    match kind {
        ObjectKind::Tree | ObjectKind::Derived => 50,
        _ => 10,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ObjectCodec {
    Raw = 0,
    Zstd = 1,
    Dict = 2,
    Delta = 3,
    Manifest = 4,
}

impl ObjectCodec {
    fn from_byte(b: u8) -> StoreResult<Self> {
        Ok(match b {
            0 => Self::Raw,
            1 => Self::Zstd,
            2 => Self::Dict,
            3 => Self::Delta,
            4 => Self::Manifest,
            c => return Err(StoreError::Corrupt(format!("unknown codec {c}"))),
        })
    }
}

fn kind_byte(kind: ObjectKind) -> u8 {
    match kind {
        ObjectKind::Content => 0,
        ObjectKind::Chunk => 1,
        ObjectKind::Tree => 2,
        ObjectKind::Changeset => 3,
        ObjectKind::Derived => 4,
    }
}

fn hash_in(kind: ObjectKind, data: &[u8]) -> [u8; 32] {
    let mut h = blake3::Hasher::new_derive_key(kind.context());
    if data.len() >= 128 << 10 {
        h.update_rayon(data);
    } else {
        h.update(data);
    }
    *h.finalize().as_bytes()
}

/// Compute the metadata stored alongside every content object.
pub fn content_meta(data: &[u8]) -> ContentMeta {
    let mut m = MetaHasher::new(data.len() as u64);
    m.update(data);
    m.finish()
}

/// Streaming [`ContentMeta`] computation (the size must be known up front
/// because git's blob hash is prefixed with it).
pub struct MetaHasher {
    size: u64,
    seen: u64,
    git: sha1::Sha1,
    is_binary: bool,
}

impl MetaHasher {
    pub fn new(size: u64) -> Self {
        let mut git = sha1::Sha1::new();
        git.update(format!("blob {size}\0").as_bytes());
        Self { size, seen: 0, git, is_binary: false }
    }

    pub fn update(&mut self, data: &[u8]) {
        if self.seen < 8192 {
            let probe = &data[..data.len().min((8192 - self.seen) as usize)];
            self.is_binary |= probe.contains(&0);
        }
        self.seen += data.len() as u64;
        self.git.update(data);
    }

    pub fn finish(self) -> ContentMeta {
        ContentMeta {
            size: self.size,
            git_sha1: self.git.finalize().into(),
            // Computed on demand (it is only served to API callers).
            sha256: [0; 32],
            is_binary: self.is_binary,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Header {
    codec: ObjectCodec,
    kind: u8,
    /// Delta chain depth (0 for whole objects); the dictionary id for
    /// codec 2.
    depth: u16,
    len: u64,
}

impl Header {
    fn parse(raw: &[u8]) -> StoreResult<Self> {
        if raw.len() < HEADER_LEN || &raw[0..4] != MAGIC {
            return Err(StoreError::Corrupt("bad object header".into()));
        }
        Ok(Self {
            codec: ObjectCodec::from_byte(raw[4])?,
            kind: raw[5],
            depth: u16::from_be_bytes(raw[6..8].try_into().unwrap()),
            len: u64::from_be_bytes(raw[8..16].try_into().unwrap()),
        })
    }

    fn write(out: &mut Vec<u8>, codec: ObjectCodec, kind: ObjectKind, len: u64) {
        Self::write_depth(out, codec, kind, len, 0)
    }

    fn write_depth(out: &mut Vec<u8>, codec: ObjectCodec, kind: ObjectKind, len: u64, depth: u16) {
        out.extend_from_slice(MAGIC);
        out.push(codec as u8);
        out.push(kind_byte(kind));
        out.extend_from_slice(&depth.to_be_bytes());
        out.extend_from_slice(&len.to_be_bytes());
    }
}

enum Backend {
    Segments(Arc<SegmentStore>),
    Object { store: Arc<dyn ObjectStore>, namespace: String },
}

struct Inner {
    backend: Backend,
    trees: moka::future::Cache<TreeId, Arc<Tree>>,
    /// Decoded, verified objects.
    decoded: moka::sync::Cache<[u8; 32], Bytes>,
    /// Trained dictionaries, loaded lazily (and reloaded when an object names
    /// one this process has not seen, e.g. trained by another server).
    dicts: std::sync::RwLock<DictSet>,
    dicts_loaded: tokio::sync::Mutex<Option<std::time::Instant>>,
}

#[derive(Clone)]
pub struct BlobStore {
    inner: Arc<Inner>,
}

/// An object to write. `base` is a hint naming a previous version of the
/// same logical object (e.g. the same path in the parent); the store may
/// delta-encode against it.
#[derive(Debug, Clone)]
pub struct PutObject {
    pub kind: ObjectKind,
    pub id: [u8; 32],
    pub data: Bytes,
    pub base: Option<[u8; 32]>,
}

/// How a content object is laid out, for streaming reads.
pub enum ContentLayout {
    Whole(Bytes),
    Chunked { size: u64, chunks: Vec<(ChunkId, u32)> },
}

#[derive(Debug, Clone, Copy, Default)]
pub struct BlobStats {
    pub objects: usize,
    pub bytes: u64,
}

impl BlobStore {
    /// Open from a URL: `file:///abs/dir` (segment store), `memory://`,
    /// `s3://bucket/prefix` (credentials and endpoint from the usual `AWS_*`
    /// env vars, so MinIO and R2 work via `AWS_ENDPOINT`).
    pub fn open(url: &str) -> anyhow::Result<Self> {
        let parsed = url::Url::parse(url)?;
        let backend = match parsed.scheme() {
            "file" => {
                let dir = parsed.to_file_path().map_err(|_| anyhow::anyhow!("bad file url {url:?}"))?;
                Backend::Segments(Arc::new(SegmentStore::open(dir)?))
            }
            "memory" => Backend::Object { store: Arc::new(object_store::memory::InMemory::new()), namespace: String::new() },
            _ => {
                let opts: Vec<(String, String)> =
                    std::env::vars().filter(|(k, _)| k.starts_with("AWS_")).map(|(k, v)| (k.to_ascii_lowercase(), v)).collect();
                let (store, path) = object_store::parse_url_opts(&parsed, opts)?;
                Backend::Object { store: Arc::from(store), namespace: path.to_string() }
            }
        };
        Ok(Self::with_backend(backend))
    }

    pub fn memory() -> Self {
        Self::with_backend(Backend::Object { store: Arc::new(object_store::memory::InMemory::new()), namespace: String::new() })
    }

    pub fn object_store(store: Arc<dyn ObjectStore>, namespace: String) -> Self {
        Self::with_backend(Backend::Object { store, namespace })
    }

    fn with_backend(backend: Backend) -> Self {
        Self {
            inner: Arc::new(Inner {
                backend,
                trees: moka::future::Cache::builder()
                    .weigher(|_, t: &Arc<Tree>| (t.entries().len() as u32).saturating_mul(96).max(64))
                    .max_capacity(256 << 20)
                    .build(),
                decoded: moka::sync::Cache::builder()
                    .weigher(|_, b: &Bytes| b.len().max(64) as u32)
                    .max_capacity(512 << 20)
                    .build(),
                dicts: Default::default(),
                dicts_loaded: Default::default(),
            }),
        }
    }

    fn key(namespace: &str, kind: ObjectKind, id: &[u8; 32]) -> ObjPath {
        let h = hex::encode(id);
        let rel = format!("{}/{}/{}/{}", kind.store_dir(), &h[0..2], &h[2..4], h);
        if namespace.is_empty() { ObjPath::from(rel) } else { ObjPath::from(format!("{namespace}/{rel}")) }
    }

    /// Snapshot local blobs into `dest` (segment backend only; object-store
    /// backends rely on the provider's durability and versioning).
    pub fn snapshot_to(&self, dest: &std::path::Path) -> anyhow::Result<BlobStats> {
        match &self.inner.backend {
            Backend::Segments(s) => {
                let st = s.snapshot_to(dest)?;
                Ok(BlobStats { objects: st.objects, bytes: st.bytes })
            }
            Backend::Object { .. } => {
                anyhow::bail!("blob snapshots are only supported for file:// stores; use bucket versioning or replication")
            }
        }
    }

    pub fn stats(&self) -> BlobStats {
        match &self.inner.backend {
            Backend::Segments(s) => {
                let st = s.stats();
                BlobStats { objects: st.objects, bytes: st.bytes }
            }
            Backend::Object { .. } => BlobStats::default(),
        }
    }

    // ───────────── backend primitives (encoded bytes) ─────────────

    async fn backend_get(&self, kind: ObjectKind, id: &[u8; 32]) -> StoreResult<Option<Bytes>> {
        match &self.inner.backend {
            Backend::Segments(s) => {
                let s = s.clone();
                let id = *id;
                let got = tokio::task::spawn_blocking(move || s.get(&id))
                    .await
                    .map_err(|e| StoreError::Backend(e.into()))?
                    .map_err(|e| StoreError::Backend(e.into()))?;
                match got {
                    Some((k, payload)) if k == kind_byte(kind) => Ok(Some(Bytes::from(payload))),
                    Some((k, _)) => Err(StoreError::Corrupt(format!("object kind mismatch: {k} vs {kind:?}"))),
                    None => Ok(None),
                }
            }
            Backend::Object { store, namespace } => match store.get(&Self::key(namespace, kind, id)).await {
                Ok(r) => Ok(Some(r.bytes().await?)),
                Err(object_store::Error::NotFound { .. }) => Ok(None),
                Err(e) => Err(e.into()),
            },
        }
    }

    /// First bytes of an encoded object (header + delta base id).
    async fn backend_peek(&self, kind: ObjectKind, id: &[u8; 32]) -> StoreResult<Option<Bytes>> {
        const N: usize = HEADER_LEN + 32;
        match &self.inner.backend {
            Backend::Segments(s) => Ok(s.get_prefix(id, N).map_err(|e| StoreError::Backend(e.into()))?.map(Bytes::from)),
            Backend::Object { store, namespace } => {
                let key = Self::key(namespace, kind, id);
                match store.get_range(&key, 0..N as u64).await {
                    Ok(b) => Ok(Some(b)),
                    Err(object_store::Error::NotFound { .. }) => Ok(None),
                    // Short objects: fall back to a full read.
                    Err(_) => self.backend_get(kind, id).await,
                }
            }
        }
    }

    async fn backend_put(&self, items: Vec<(ObjectKind, [u8; 32], Vec<u8>)>) -> StoreResult<()> {
        if items.is_empty() {
            return Ok(());
        }
        match &self.inner.backend {
            Backend::Segments(s) => {
                let s = s.clone();
                tokio::task::spawn_blocking(move || {
                    let refs: Vec<(u8, [u8; 32], &[u8])> =
                        items.iter().map(|(k, id, d)| (kind_byte(*k), *id, d.as_slice())).collect();
                    s.put_batch(&refs)
                })
                .await
                .map_err(|e| StoreError::Backend(e.into()))?
                .map_err(|e| StoreError::Backend(e.into()))
            }
            Backend::Object { store, namespace } => {
                let results: Vec<StoreResult<()>> = futures::stream::iter(items)
                    .map(|(kind, id, data)| {
                        let store = store.clone();
                        let key = Self::key(namespace, kind, &id);
                        async move {
                            store.put(&key, PutPayload::from(Bytes::from(data))).await?;
                            Ok(())
                        }
                    })
                    .buffer_unordered(32)
                    .collect()
                    .await;
                results.into_iter().collect()
            }
        }
    }

    pub async fn exists(&self, kind: ObjectKind, id: &[u8; 32]) -> StoreResult<bool> {
        if self.inner.decoded.contains_key(id) {
            return Ok(true);
        }
        match &self.inner.backend {
            Backend::Segments(s) => Ok(s.contains(id)),
            Backend::Object { store, namespace } => match store.head(&Self::key(namespace, kind, id)).await {
                Ok(_) => Ok(true),
                Err(object_store::Error::NotFound { .. }) => Ok(false),
                Err(e) => Err(e.into()),
            },
        }
    }

    /// Stored bytes by `(kind, codec)`: `(objects, encoded bytes, decoded
    /// bytes)`. Reads every object header; segment stores only.
    pub async fn codec_stats(&self) -> StoreResult<std::collections::BTreeMap<(String, String), (u64, u64, u64)>> {
        let Backend::Segments(s) = &self.inner.backend else {
            return Err(StoreError::Backend(anyhow::anyhow!("storage stats are only available for file:// stores")));
        };
        let s = s.clone();
        tokio::task::spawn_blocking(move || -> StoreResult<_> {
            let mut out: std::collections::BTreeMap<(String, String), (u64, u64, u64)> = Default::default();
            for id in s.ids() {
                let Some((kind, raw)) = s.get(&id).map_err(|e| StoreError::Backend(e.into()))? else { continue };
                let h = Header::parse(&raw)?;
                let kind = ["content", "chunk", "tree", "changeset", "derived"].get(kind as usize).copied().unwrap_or("?");
                let codec = match h.codec {
                    ObjectCodec::Raw => "raw",
                    ObjectCodec::Zstd => "zstd",
                    ObjectCodec::Dict => "zstd+dict",
                    ObjectCodec::Delta => "delta",
                    ObjectCodec::Manifest => "chunked",
                };
                let e = out.entry((kind.to_string(), codec.to_string())).or_default();
                e.0 += 1;
                e.1 += raw.len() as u64;
                e.2 += h.len;
            }
            Ok(out)
        })
        .await
        .map_err(|e| StoreError::Backend(e.into()))?
    }

    // ───────────── trained dictionaries ─────────────

    fn dict_key(namespace: &str, id: u16) -> ObjPath {
        if namespace.is_empty() { ObjPath::from(format!("dicts/{id}")) } else { ObjPath::from(format!("{namespace}/dicts/{id}")) }
    }

    /// Read every dictionary file from the backend.
    async fn read_dict_files(&self) -> StoreResult<Vec<Vec<u8>>> {
        match &self.inner.backend {
            Backend::Segments(s) => {
                let dir = s.dir().join("dicts");
                tokio::task::spawn_blocking(move || -> std::io::Result<Vec<Vec<u8>>> {
                    let mut out = Vec::new();
                    let entries = match std::fs::read_dir(&dir) {
                        Ok(e) => e,
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
                        Err(e) => return Err(e),
                    };
                    for e in entries {
                        let path = e?.path();
                        if path.extension().is_some_and(|x| x == "zdict") {
                            out.push(std::fs::read(path)?);
                        }
                    }
                    Ok(out)
                })
                .await
                .map_err(|e| StoreError::Backend(e.into()))?
                .map_err(|e| StoreError::Backend(e.into()))
            }
            Backend::Object { store, namespace } => {
                let prefix =
                    if namespace.is_empty() { ObjPath::from("dicts") } else { ObjPath::from(format!("{namespace}/dicts")) };
                let metas: Vec<object_store::ObjectMeta> = store
                    .list(Some(&prefix))
                    .map(|m| m.map_err(StoreError::from))
                    .collect::<Vec<_>>()
                    .await
                    .into_iter()
                    .collect::<StoreResult<_>>()?;
                let mut out = Vec::new();
                for m in metas {
                    out.push(store.get(&m.location).await?.bytes().await?.to_vec());
                }
                Ok(out)
            }
        }
    }

    /// (Re)load dictionaries from the backend.
    pub async fn load_dicts(&self) -> StoreResult<()> {
        let mut loaded = self.inner.dicts_loaded.lock().await;
        let files = self.read_dict_files().await?;
        let mut set = self.inner.dicts.read().unwrap().clone();
        for f in files {
            let d = Dict::decode_file(&f, ZSTD_LEVEL)?;
            if !set.by_id.contains_key(&d.id) {
                set.insert(d);
            }
        }
        *self.inner.dicts.write().unwrap() = set;
        *loaded = Some(std::time::Instant::now());
        Ok(())
    }

    /// The newest dictionary per kind (loading dictionaries on first use and
    /// rechecking every minute for ones trained elsewhere).
    async fn active_dicts(&self) -> HashMap<ObjectKind, Arc<Dict>> {
        let stale = self.inner.dicts_loaded.lock().await.is_none_or(|t| t.elapsed() > std::time::Duration::from_secs(60));
        if stale && let Err(e) = self.load_dicts().await {
            tracing::warn!(error = %e, "loading compression dictionaries failed");
        }
        let set = self.inner.dicts.read().unwrap();
        [ObjectKind::Content, ObjectKind::Tree, ObjectKind::Changeset, ObjectKind::Derived]
            .into_iter()
            .filter_map(|k| set.active(k).map(|d| (k, d.clone())))
            .collect()
    }

    async fn dict(&self, id: u16) -> StoreResult<Arc<Dict>> {
        if let Some(d) = self.inner.dicts.read().unwrap().get(id) {
            return Ok(d.clone());
        }
        self.load_dicts().await?;
        self.inner.dicts.read().unwrap().get(id).cloned().ok_or_else(|| StoreError::Corrupt(format!("dictionary {id} missing")))
    }

    /// Dictionaries known to this store: `(id, kind, bytes)`.
    pub async fn list_dicts(&self) -> StoreResult<Vec<(u16, ObjectKind, usize)>> {
        self.load_dicts().await?;
        let mut out: Vec<_> = self.inner.dicts.read().unwrap().by_id.values().map(|d| (d.id, d.kind, d.raw.len())).collect();
        out.sort_by_key(|d| d.0);
        Ok(out)
    }

    /// Persist a newly trained dictionary and make it the active one for its
    /// kind. Dictionary files are never overwritten.
    pub async fn install_dict(&self, kind: ObjectKind, raw: Vec<u8>) -> StoreResult<u16> {
        self.load_dicts().await?;
        let id = self.inner.dicts.read().unwrap().next_id();
        let dict = Dict::new(id, kind, raw, ZSTD_LEVEL);
        let file = dict.encode_file();
        match &self.inner.backend {
            Backend::Segments(s) => {
                let dir = s.dir().join("dicts");
                tokio::task::spawn_blocking(move || -> std::io::Result<()> {
                    use std::io::Write as _;
                    std::fs::create_dir_all(&dir)?;
                    let tmp = dir.join(format!("{id}.zdict.tmp"));
                    let mut f = std::fs::File::create(&tmp)?;
                    f.write_all(&file)?;
                    f.sync_all()?;
                    // A hard link creates the name atomically and fails if it exists.
                    let linked = std::fs::hard_link(&tmp, dir.join(format!("{id}.zdict")));
                    std::fs::remove_file(&tmp)?;
                    linked?;
                    std::fs::File::open(&dir)?.sync_all()
                })
                .await
                .map_err(|e| StoreError::Backend(e.into()))?
                .map_err(|e| StoreError::Backend(e.into()))?;
            }
            Backend::Object { store, namespace } => {
                let opts = object_store::PutOptions { mode: object_store::PutMode::Create, ..Default::default() };
                store.put_opts(&Self::dict_key(namespace, id), PutPayload::from(Bytes::from(file)), opts).await?;
            }
        }
        self.inner.dicts.write().unwrap().insert(dict);
        Ok(id)
    }

    /// Make all writes so far durable. MUST be called before metadata that
    /// references new objects is committed (SPEC §5.2 ordering invariant).
    pub async fn sync(&self) -> StoreResult<()> {
        if let Backend::Segments(s) = &self.inner.backend {
            let s = s.clone();
            tokio::task::spawn_blocking(move || s.sync())
                .await
                .map_err(|e| StoreError::Backend(e.into()))?
                .map_err(|e| StoreError::Backend(e.into()))?;
        }
        Ok(())
    }

    // ───────────── decoding ─────────────

    /// Fetch, decode and verify an object.
    pub async fn get_raw(&self, kind: ObjectKind, id: &[u8; 32]) -> StoreResult<Option<Bytes>> {
        self.get_decoded(kind, *id, 0).await
    }

    fn get_decoded(
        &self,
        kind: ObjectKind,
        id: [u8; 32],
        depth: u32,
    ) -> futures::future::BoxFuture<'_, StoreResult<Option<Bytes>>> {
        Box::pin(async move {
            let id = &id;
            if let Some(b) = self.inner.decoded.get(id) {
                return Ok(Some(b));
            }
            if depth > 64 {
                return Err(StoreError::Corrupt("delta chain too deep".into()));
            }
            let Some(raw) = self.backend_get(kind, id).await? else {
                return Ok(None);
            };
            let h = Header::parse(&raw)?;
            if h.kind != kind_byte(kind) {
                return Err(StoreError::Corrupt(format!("object kind mismatch: {} vs {kind:?}", h.kind)));
            }
            let payload = raw.slice(HEADER_LEN..);
            let len = h.len as usize;
            let data: Bytes = match h.codec {
                ObjectCodec::Raw => payload,
                ObjectCodec::Zstd => {
                    let out = block_in_place_if_large(len, || zstd::bulk::decompress(&payload, len))
                        .map_err(|e| StoreError::Corrupt(format!("zstd: {e}")))?;
                    Bytes::from(out)
                }
                ObjectCodec::Dict => {
                    let dict = self.dict(h.depth).await?;
                    let out = block_in_place_if_large(len, || {
                        zstd::bulk::Decompressor::with_prepared_dictionary(&dict.dec)
                            .and_then(|mut d| d.decompress(&payload, len))
                    })
                    .map_err(|e| StoreError::Corrupt(format!("zstd dict {}: {e}", h.depth)))?;
                    Bytes::from(out)
                }
                ObjectCodec::Delta => {
                    if payload.len() < 32 {
                        return Err(StoreError::Corrupt("short delta".into()));
                    }
                    let base_id: [u8; 32] = payload[..32].try_into().unwrap();
                    let base = self
                        .get_decoded(kind, base_id, depth + 1)
                        .await?
                        .ok_or_else(|| StoreError::Corrupt(format!("delta base {} missing", hex::encode(base_id))))?;
                    let mut d = zstd::bulk::Decompressor::with_dictionary(&base)
                        .map_err(|e| StoreError::Corrupt(format!("zstd: {e}")))?;
                    Bytes::from(d.decompress(&payload[32..], len).map_err(|e| StoreError::Corrupt(format!("zstd delta: {e}")))?)
                }
                ObjectCodec::Manifest => {
                    if kind != ObjectKind::Content {
                        return Err(StoreError::Corrupt("manifest on non-content object".into()));
                    }
                    let chunks = parse_manifest(&payload)?;
                    let futs: Vec<_> =
                        chunks.iter().map(|(cid, _)| self.get_decoded(ObjectKind::Chunk, *cid.as_bytes(), depth + 1)).collect();
                    let parts: Vec<StoreResult<Option<Bytes>>> = futures::stream::iter(futs).buffered(16).collect().await;
                    let mut out = Vec::with_capacity(len);
                    for (p, (cid, _)) in parts.into_iter().zip(&chunks) {
                        out.extend_from_slice(&p?.ok_or_else(|| StoreError::Corrupt(format!("chunk {cid} missing")))?);
                    }
                    Bytes::from(out)
                }
            };
            if data.len() != len {
                return Err(StoreError::Corrupt("length mismatch".into()));
            }
            if hash_in(kind, &data) != *id {
                return Err(StoreError::Corrupt(format!("{kind:?} hash mismatch for {}", hex::encode(id))));
            }
            if data.len() <= CACHE_OBJECT_MAX {
                self.inner.decoded.insert(*id, data.clone());
            }
            Ok(Some(data))
        })
    }

    // ───────────── encoding ─────────────

    /// Delta base for a hint: its decoded bytes and chain depth, if the base
    /// exists and another link fits under the kind's depth cap.
    async fn delta_base(&self, kind: ObjectKind, base: &[u8; 32]) -> StoreResult<Option<(u16, Bytes)>> {
        let Some(peek) = self.backend_peek(kind, base).await? else {
            return Ok(None);
        };
        let h = Header::parse(&peek)?;
        let depth = match h.codec {
            ObjectCodec::Raw | ObjectCodec::Zstd | ObjectCodec::Dict => 0,
            ObjectCodec::Delta => h.depth,
            ObjectCodec::Manifest => return Ok(None),
        };
        if depth + 1 > max_depth(kind) || h.len as usize > MAX_DELTA_BYTES {
            return Ok(None);
        }
        Ok(self.get_decoded(kind, *base, 0).await?.map(|b| (depth, b)))
    }

    /// Write objects, delta-encoding against `base` hints where it pays off.
    /// Idempotent; objects already present are skipped.
    pub async fn put_objects(&self, items: Vec<PutObject>) -> StoreResult<()> {
        let mut seen = HashSet::new();
        let mut todo: Vec<PutObject> = Vec::with_capacity(items.len());
        for it in items {
            if seen.insert(it.id) && !self.exists(it.kind, &it.id).await? {
                todo.push(it);
            }
        }
        if todo.is_empty() {
            return Ok(());
        }
        // Plan every item's delta base and depth before compressing anything,
        // so the whole batch encodes in one parallel pass even when items
        // chain through each other (e.g. 500 versions of a root tree). An
        // in-batch base that ends up stored whole only makes the planned
        // depth an overestimate, which is safe: depth just caps chains.
        let index: HashMap<[u8; 32], usize> = todo.iter().enumerate().map(|(i, it)| (it.id, i)).collect();
        let mut planned: Vec<Option<u16>> = vec![None; todo.len()];
        let mut bases: Vec<Option<([u8; 32], u16, Bytes)>> = vec![None; todo.len()];
        fn plan(
            i: usize,
            todo: &[PutObject],
            index: &HashMap<[u8; 32], usize>,
            planned: &mut [Option<u16>],
            guard: usize,
        ) -> Option<u16> {
            if let Some(d) = planned[i] {
                return Some(d);
            }
            // Depth of an in-batch base, resolved iteratively up the chain.
            let it = &todo[i];
            let d = match it.base.and_then(|b| index.get(&b).copied()) {
                Some(j) if j != i && guard < todo.len() => plan(j, todo, index, planned, guard + 1).map(|bd| bd + 1),
                _ => None,
            };
            planned[i] = Some(match d {
                Some(d) if d <= max_depth(it.kind) => d,
                _ => 0,
            });
            planned[i]
        }
        let usable = |it: &PutObject| it.data.len() <= MAX_DELTA_BYTES && it.data.len() >= MIN_COMPRESS;
        // External bases first: their stored depth seeds the plan. They are
        // fetched concurrently (each may decode a delta chain).
        let external: Vec<(usize, [u8; 32])> = todo
            .iter()
            .enumerate()
            .filter_map(|(i, it)| match it.base.filter(|b| *b != it.id) {
                Some(b) if !index.contains_key(&b) && usable(it) => Some((i, b)),
                _ => None,
            })
            .collect();
        let futs: Vec<_> = external
            .iter()
            .map(|(i, b)| {
                let (kind, b) = (todo[*i].kind, *b);
                async move { self.delta_base(kind, &b).await }
            })
            .collect();
        let fetched: Vec<StoreResult<Option<(u16, Bytes)>>> = futures::stream::iter(futs).buffered(32).collect().await;
        for ((i, b), base) in external.iter().zip(fetched) {
            let base = base?.filter(|(_, bytes)| !bytes.starts_with(&ZSTD_DICT_MAGIC));
            planned[*i] = Some(base.as_ref().map(|(d, _)| d + 1).unwrap_or(0));
            bases[*i] = base.map(|(d, bytes)| (*b, d, bytes));
        }
        for (i, it) in todo.iter().enumerate() {
            match it.base.filter(|b| *b != it.id) {
                Some(b) if !index.contains_key(&b) && planned[i].is_none() => planned[i] = Some(0),
                None => planned[i] = Some(0),
                _ => {}
            }
        }
        for i in 0..todo.len() {
            plan(i, &todo, &index, &mut planned, 0);
        }
        for (i, it) in todo.iter().enumerate() {
            let Some(b) = it.base.filter(|b| *b != it.id) else { continue };
            if let Some(&j) = index.get(&b)
                && usable(it)
                && planned[i].unwrap_or(0) > 0
                && !todo[j].data.starts_with(&ZSTD_DICT_MAGIC)
            {
                bases[i] = Some((b, planned[j].unwrap_or(0), todo[j].data.clone()));
            } else if index.contains_key(&b) {
                planned[i] = Some(0);
            }
        }
        let base_ids: Vec<Option<[u8; 32]>> = bases.iter().map(|b| b.as_ref().map(|(id, _, _)| *id)).collect();
        let ext_depth: Vec<Option<u16>> =
            bases.iter().map(|b| b.as_ref().filter(|(id, _, _)| !index.contains_key(id)).map(|(_, d, _)| *d)).collect();
        // New versions usually become the next version's delta base: keep
        // them decoded (once written) so that needs no chain walk.
        let keep: Vec<([u8; 32], Bytes)> = todo
            .iter()
            .filter(|it| it.kind == ObjectKind::Content && (MIN_COMPRESS..=CACHE_OBJECT_MAX).contains(&it.data.len()))
            .map(|it| (it.id, it.data.clone()))
            .collect();
        let planned_at = std::time::Instant::now();
        let work: Vec<(PutObject, Option<([u8; 32], u16, Bytes)>)> = todo.into_iter().zip(bases).collect();
        let dicts = self.active_dicts().await;
        let encoded: Vec<StoreResult<(ObjectKind, [u8; 32], Vec<u8>, bool)>> = tokio::task::spawn_blocking(move || {
            work.into_par_iter()
                .map(|(it, base)| {
                    let dict = dicts.get(&it.kind).map(|d| d.as_ref());
                    let (bytes, depth) = encode(it.kind, &it.data, base.as_ref().map(|(id, d, b)| (id, *d, b.as_ref())), dict)?;
                    Ok((it.kind, it.id, bytes, depth > 0))
                })
                .collect()
        })
        .await
        .map_err(|e| StoreError::Backend(e.into()))?;
        let mut encoded: Vec<(ObjectKind, [u8; 32], Vec<u8>, bool)> = encoded.into_iter().collect::<StoreResult<_>>()?;
        // Planned depths are upper bounds (a planned delta base may have been
        // stored whole). Record the real depths so later batches extend
        // chains as far as the cap allows.
        let mut real: Vec<Option<u16>> = vec![None; encoded.len()];
        fn real_depth(
            i: usize,
            encoded: &[(ObjectKind, [u8; 32], Vec<u8>, bool)],
            base_ids: &[Option<[u8; 32]>],
            ext: &[Option<u16>],
            index: &HashMap<[u8; 32], usize>,
            real: &mut [Option<u16>],
        ) -> u16 {
            if let Some(d) = real[i] {
                return d;
            }
            let d = if !encoded[i].3 {
                0
            } else if let Some(d) = ext[i] {
                d + 1
            } else {
                match base_ids[i].and_then(|b| index.get(&b).copied()) {
                    Some(j) if j != i => real_depth(j, encoded, base_ids, ext, index, real) + 1,
                    _ => 1,
                }
            };
            real[i] = Some(d);
            d
        }
        for i in 0..encoded.len() {
            let d = real_depth(i, &encoded, &base_ids, &ext_depth, &index, &mut real);
            if encoded[i].3 {
                encoded[i].2[6..8].copy_from_slice(&d.to_be_bytes());
            }
        }
        let encode_us = planned_at.elapsed().as_micros() as u64;
        self.backend_put(encoded.into_iter().map(|(k, id, b, _)| (k, id, b)).collect()).await?;
        tracing::debug!(n = keep.len(), encode_us, total_us = planned_at.elapsed().as_micros() as u64, "put_objects");
        for (id, data) in keep {
            self.inner.decoded.insert(id, data);
        }
        Ok(())
    }

    /// Write a single object with no delta hint.
    pub async fn put_raw(&self, kind: ObjectKind, id: &[u8; 32], data: &[u8]) -> StoreResult<()> {
        self.put_objects(vec![PutObject { kind, id: *id, data: Bytes::copy_from_slice(data), base: None }]).await
    }

    /// Store an already-encoded object exactly as given (tests, repair).
    #[doc(hidden)]
    pub async fn put_encoded_unchecked(&self, kind: ObjectKind, id: &[u8; 32], data: &[u8]) -> StoreResult<()> {
        let mut out = Vec::with_capacity(HEADER_LEN + data.len());
        Header::write(&mut out, ObjectCodec::Raw, kind, data.len() as u64);
        out.extend_from_slice(data);
        self.backend_put(vec![(kind, *id, out)]).await
    }

    // ───────────── contents ─────────────

    /// Store file contents; returns ids and metadata in input order. Each
    /// item may carry a delta hint (the previous version of the same path).
    /// Large contents are chunked. The caller records metadata in the
    /// [`crate::MetaStore`] after [`BlobStore::sync`] (SPEC §5.2 ordering).
    pub async fn put_contents(&self, items: Vec<(Bytes, Option<ContentId>)>) -> StoreResult<Vec<(ContentId, ContentMeta)>> {
        let hashed: Vec<(Bytes, Option<ContentId>, ContentId, ContentMeta)> = tokio::task::spawn_blocking(move || {
            items
                .into_par_iter()
                .map(|(data, base)| {
                    let id = ContentId::hash(&data);
                    let meta = content_meta(&data);
                    (data, base, id, meta)
                })
                .collect()
        })
        .await
        .map_err(|e| StoreError::Backend(e.into()))?;
        let mut objects = Vec::new();
        let mut out = Vec::with_capacity(hashed.len());
        for (data, base, id, meta) in hashed {
            out.push((id, meta));
            if data.len() > CHUNK_THRESHOLD {
                if !self.exists(ObjectKind::Content, id.as_bytes()).await? {
                    self.put_chunked(id, data).await?;
                }
            } else {
                objects.push(PutObject {
                    kind: ObjectKind::Content,
                    id: *id.as_bytes(),
                    data,
                    base: base.map(|b| *b.as_bytes()),
                });
            }
        }
        self.put_objects(objects).await?;
        Ok(out)
    }

    pub async fn put_content(&self, data: &[u8]) -> StoreResult<(ContentId, ContentMeta)> {
        Ok(self.put_contents(vec![(Bytes::copy_from_slice(data), None)]).await?.pop().unwrap())
    }

    async fn put_chunked(&self, id: ContentId, data: Bytes) -> StoreResult<()> {
        let mut sink = ChunkSink::new(self.clone());
        sink.push(&data).await?;
        sink.finish(id, data.len() as u64).await
    }

    /// A sink for streaming very large content into chunks.
    pub fn chunk_sink(&self) -> ChunkSink {
        ChunkSink::new(self.clone())
    }

    /// Fetch content, verifying its hash.
    pub async fn get_content(&self, id: &ContentId) -> StoreResult<Option<Vec<u8>>> {
        Ok(self.get_content_bytes(id).await?.map(|b| b.to_vec()))
    }

    pub async fn get_content_bytes(&self, id: &ContentId) -> StoreResult<Option<Bytes>> {
        self.get_decoded(ObjectKind::Content, *id.as_bytes(), 0).await
    }

    /// For streaming reads: whole bytes for small content, the chunk list for
    /// chunked content (each chunk is fetched with [`BlobStore::get_chunk`]).
    pub async fn content_layout(&self, id: &ContentId) -> StoreResult<Option<ContentLayout>> {
        if let Some(b) = self.inner.decoded.get(id.as_bytes()) {
            return Ok(Some(ContentLayout::Whole(b)));
        }
        let Some(peek) = self.backend_peek(ObjectKind::Content, id.as_bytes()).await? else {
            return Ok(None);
        };
        let h = Header::parse(&peek)?;
        if h.codec != ObjectCodec::Manifest {
            return Ok(self.get_content_bytes(id).await?.map(ContentLayout::Whole));
        }
        let raw = self
            .backend_get(ObjectKind::Content, id.as_bytes())
            .await?
            .ok_or_else(|| StoreError::Corrupt("manifest vanished".into()))?;
        Ok(Some(ContentLayout::Chunked { size: h.len, chunks: parse_manifest(&raw[HEADER_LEN..])? }))
    }

    pub async fn get_chunk(&self, id: &ChunkId) -> StoreResult<Option<Bytes>> {
        self.get_decoded(ObjectKind::Chunk, *id.as_bytes(), 0).await
    }

    // ───────────── trees ─────────────

    /// Write trees; each may name the previous version of the same directory
    /// as a delta base.
    pub async fn put_trees(&self, trees: Vec<(Arc<Tree>, Option<TreeId>)>) -> StoreResult<Vec<TreeId>> {
        let mut ids = Vec::with_capacity(trees.len());
        let mut objects = Vec::with_capacity(trees.len());
        for (tree, base) in trees {
            let bytes = tree.encode();
            let id = TreeId::hash(&bytes);
            ids.push(id);
            if self.inner.trees.contains_key(&id) {
                continue;
            }
            objects.push(PutObject {
                kind: ObjectKind::Tree,
                id: *id.as_bytes(),
                data: Bytes::from(bytes),
                base: base.map(|b| *b.as_bytes()),
            });
            self.inner.trees.insert(id, tree).await;
        }
        self.put_objects(objects).await?;
        Ok(ids)
    }

    /// Like [`BlobStore::put_trees`] for trees whose canonical bytes and ids
    /// the caller already computed: `(id, tree, bytes, base)`.
    pub async fn put_trees_encoded(&self, trees: Vec<(TreeId, Arc<Tree>, Bytes, Option<TreeId>)>) -> StoreResult<()> {
        let mut objects = Vec::with_capacity(trees.len());
        for (id, tree, bytes, base) in trees {
            if self.inner.trees.contains_key(&id) && self.exists(ObjectKind::Tree, id.as_bytes()).await? {
                continue;
            }
            objects.push(PutObject {
                kind: ObjectKind::Tree,
                id: *id.as_bytes(),
                data: bytes,
                base: base.map(|b| *b.as_bytes()),
            });
            self.inner.trees.insert(id, tree).await;
        }
        self.put_objects(objects).await
    }

    pub async fn put_tree(&self, tree: &Tree) -> StoreResult<TreeId> {
        Ok(self.put_trees(vec![(Arc::new(tree.clone()), None)]).await?.pop().unwrap())
    }

    pub async fn get_tree(&self, id: &TreeId) -> StoreResult<Option<Arc<Tree>>> {
        if let Some(t) = self.inner.trees.get(id).await {
            return Ok(Some(t));
        }
        let Some(bytes) = self.get_decoded(ObjectKind::Tree, *id.as_bytes(), 0).await? else {
            return Ok(None);
        };
        let tree = Arc::new(Tree::decode(&bytes).map_err(|e| StoreError::Corrupt(e.to_string()))?);
        self.inner.trees.insert(*id, tree.clone()).await;
        Ok(Some(tree))
    }

    /// Canonical tree bytes (what clients hash and cache).
    pub async fn get_tree_bytes(&self, id: &TreeId) -> StoreResult<Option<Bytes>> {
        self.get_decoded(ObjectKind::Tree, *id.as_bytes(), 0).await
    }
}

/// Decompressing more than a few MiB inline would stall the reactor.
fn block_in_place_if_large<R>(len: usize, f: impl FnOnce() -> R) -> R {
    let multi =
        tokio::runtime::Handle::try_current().is_ok_and(|h| h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread);
    if len > (1 << 20) && multi { tokio::task::block_in_place(f) } else { f() }
}

/// zstd-compress `data` with `base` as a single-use raw-content prefix:
/// indexed in place rather than copied into a dictionary first. Frames
/// decode with `base` as a raw-content dictionary.
fn delta_compress(base: &[u8], data: &[u8]) -> std::io::Result<Vec<u8>> {
    use zstd::zstd_safe::{CCtx, CParameter};
    let code = |c: usize| std::io::Error::other(zstd::zstd_safe::get_error_name(c));
    let mut cctx = CCtx::create();
    cctx.set_parameter(CParameter::CompressionLevel(ZSTD_DELTA_LEVEL)).map_err(code)?;
    cctx.ref_prefix(base).map_err(code)?;
    let mut out = Vec::with_capacity(zstd::zstd_safe::compress_bound(data.len()));
    cctx.compress2(&mut out, data).map_err(code)?;
    Ok(out)
}

/// Encode one object against an optional delta base `(id, depth, bytes)`.
/// Returns the encoded bytes and the resulting chain depth.
fn encode(
    kind: ObjectKind,
    data: &[u8],
    base: Option<(&[u8; 32], u16, &[u8])>,
    dict: Option<&Dict>,
) -> StoreResult<(Vec<u8>, u16)> {
    let err = |e: std::io::Error| StoreError::Backend(e.into());
    let mut out = Vec::with_capacity(HEADER_LEN + data.len() / 2);
    if data.len() < MIN_COMPRESS {
        Header::write(&mut out, ObjectCodec::Raw, kind, data.len() as u64);
        out.extend_from_slice(data);
        return Ok((out, 0));
    }
    let delta = match base {
        Some((bid, depth, bytes)) => Some((bid, depth + 1, delta_compress(bytes, data).map_err(err)?)),
        None => None,
    };
    let write_delta = |out: &mut Vec<u8>, bid: &[u8; 32], depth: u16, d: &[u8]| {
        Header::write_depth(out, ObjectCodec::Delta, kind, data.len() as u64, depth);
        out.extend_from_slice(bid);
        out.extend_from_slice(d);
    };
    // A small delta is good enough: standalone compression rarely gets
    // below a quarter, so this already beats it by the 2x required below.
    if let Some((bid, depth, d)) = &delta
        && d.len() + 32 <= data.len() / 8
    {
        write_delta(&mut out, bid, *depth, d);
        return Ok((out, *depth));
    }
    // Small objects: the kind's trained dictionary (when there is one).
    let (full, full_codec, dict_id) = match dict.filter(|_| data.len() <= DICT_MAX_OBJECT) {
        Some(d) => (
            zstd::bulk::Compressor::with_prepared_dictionary(&d.enc).and_then(|mut c| c.compress(data)).map_err(err)?,
            ObjectCodec::Dict,
            d.id,
        ),
        None => (zstd::bulk::compress(data, ZSTD_LEVEL).map_err(err)?, ObjectCodec::Zstd, 0),
    };
    match delta {
        // Deltas must clearly beat standalone compression.
        Some((bid, depth, d)) if d.len() + 32 < full.len() / 2 => {
            write_delta(&mut out, bid, depth, &d);
            Ok((out, depth))
        }
        _ if full.len() < data.len() => {
            Header::write_depth(&mut out, full_codec, kind, data.len() as u64, dict_id);
            out.extend_from_slice(&full);
            Ok((out, 0))
        }
        _ => {
            Header::write(&mut out, ObjectCodec::Raw, kind, data.len() as u64);
            out.extend_from_slice(data);
            Ok((out, 0))
        }
    }
}

fn parse_manifest(payload: &[u8]) -> StoreResult<Vec<(ChunkId, u32)>> {
    if payload.len() < 4 {
        return Err(StoreError::Corrupt("short manifest".into()));
    }
    let n = u32::from_be_bytes(payload[0..4].try_into().unwrap()) as usize;
    if payload.len() != 4 + n * 36 {
        return Err(StoreError::Corrupt("manifest length mismatch".into()));
    }
    Ok((0..n)
        .map(|i| {
            let o = 4 + i * 36;
            let id = ChunkId::from_bytes(payload[o..o + 32].try_into().unwrap());
            (id, u32::from_be_bytes(payload[o + 32..o + 36].try_into().unwrap()))
        })
        .collect())
}

/// Streams content into content-defined chunks (FastCDC, SPEC §3.2). Cut
/// points depend only on the bytes, so the result is identical however the
/// input is split across `push` calls.
pub struct ChunkSink {
    store: BlobStore,
    buf: Vec<u8>,
    chunks: Vec<(ChunkId, u32)>,
    total: u64,
}

impl ChunkSink {
    fn new(store: BlobStore) -> Self {
        Self { store, buf: Vec::new(), chunks: Vec::new(), total: 0 }
    }

    pub async fn push(&mut self, data: &[u8]) -> StoreResult<()> {
        self.buf.extend_from_slice(data);
        self.total += data.len() as u64;
        // A cut is final once CHUNK_MAX bytes follow the chunk start.
        if self.buf.len() >= 4 * CHUNK_MAX as usize {
            self.cut(false).await?;
        }
        Ok(())
    }

    async fn cut(&mut self, last: bool) -> StoreResult<()> {
        let mut pos = 0usize;
        let mut objects = Vec::new();
        loop {
            let rest = &self.buf[pos..];
            if rest.is_empty() || (!last && rest.len() < CHUNK_MAX as usize) {
                break;
            }
            let len = fastcdc::v2020::FastCDC::new(rest, CHUNK_MIN, CHUNK_AVG, CHUNK_MAX)
                .next()
                .map(|c| c.length)
                .unwrap_or(rest.len());
            let chunk = Bytes::copy_from_slice(&rest[..len]);
            let id = ChunkId::hash(&chunk);
            self.chunks.push((id, len as u32));
            objects.push(PutObject { kind: ObjectKind::Chunk, id: *id.as_bytes(), data: chunk, base: None });
            pos += len;
        }
        self.buf.drain(..pos);
        self.store.put_objects(objects).await
    }

    /// Write the remaining chunks and the manifest for content `id`. The
    /// caller is responsible for having verified `id` over the full bytes.
    pub async fn finish(mut self, id: ContentId, size: u64) -> StoreResult<()> {
        if size != self.total {
            return Err(StoreError::Corrupt("chunked size mismatch".into()));
        }
        self.cut(true).await?;
        let mut out = Vec::with_capacity(HEADER_LEN + 4 + self.chunks.len() * 36);
        Header::write(&mut out, ObjectCodec::Manifest, ObjectKind::Content, size);
        out.extend_from_slice(&(self.chunks.len() as u32).to_be_bytes());
        for (cid, len) in &self.chunks {
            out.extend_from_slice(cid.as_bytes());
            out.extend_from_slice(&len.to_be_bytes());
        }
        self.store.backend_put(vec![(ObjectKind::Content, *id.as_bytes(), out)]).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flux_core::{FileKind, PathComponent, TreeEntry};

    fn stores() -> Vec<(tempfile::TempDir, BlobStore)> {
        let dir = tempfile::tempdir().unwrap();
        let url = url::Url::from_directory_path(dir.path()).unwrap();
        let seg = BlobStore::open(url.as_str()).unwrap();
        vec![(tempfile::tempdir().unwrap(), BlobStore::memory()), (dir, seg)]
    }

    #[tokio::test]
    async fn content_roundtrip_and_meta() {
        for (_d, s) in stores() {
            let data = b"hello world\n".repeat(100);
            let (id, meta) = s.put_content(&data).await.unwrap();
            assert_eq!(s.get_content(&id).await.unwrap().unwrap(), data);
            assert_eq!(meta.size, data.len() as u64);
            assert!(!meta.is_binary);
            assert!(s.get_content(&ContentId::hash(b"missing")).await.unwrap().is_none());
        }
    }

    #[test]
    fn git_blob_sha1_matches_git() {
        // `printf 'hello\n' | git hash-object --stdin`
        assert_eq!(hex::encode(content_meta(b"hello\n").git_sha1), "ce013625030ba8dba906f756967f9e9ca394464a");
        assert_eq!(hex::encode(content_meta(b"").git_sha1), "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391");
    }

    #[tokio::test]
    async fn tree_roundtrip_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let url = url::Url::from_directory_path(dir.path()).unwrap();
        let t = Tree::new(vec![TreeEntry::file(PathComponent::new("a").unwrap(), FileKind::Regular, ContentId::hash(b"a"), 1)])
            .unwrap();
        let id = {
            let s = BlobStore::open(url.as_str()).unwrap();
            let id = s.put_tree(&t).await.unwrap();
            s.sync().await.unwrap();
            id
        };
        // Fresh store instance: no in-process cache.
        let s2 = BlobStore::open(url.as_str()).unwrap();
        assert_eq!(*s2.get_tree(&id).await.unwrap().unwrap(), t);
    }

    #[tokio::test]
    async fn detects_corruption() {
        for (_d, s) in stores() {
            let id = ContentId::hash(b"real");
            s.put_encoded_unchecked(ObjectKind::Content, id.as_bytes(), b"fake").await.unwrap();
            assert!(s.get_content(&id).await.is_err());
        }
    }

    fn version(n: usize) -> Vec<u8> {
        let mut v: Vec<u8> = (0..4000).flat_map(|i| format!("line {i} of a fairly long source file\n").into_bytes()).collect();
        v.extend_from_slice(format!("edit number {n}\n").as_bytes());
        v
    }

    #[tokio::test]
    async fn delta_chains() {
        for (_d, s) in stores() {
            let mut prev: Option<ContentId> = None;
            let mut ids = vec![];
            // Several versions in one batch, each hinting the previous one.
            let batch: Vec<(Bytes, Option<ContentId>)> = (0..5)
                .map(|n| {
                    let d = Bytes::from(version(n));
                    let id = ContentId::hash(&d);
                    let item = (d, prev);
                    prev = Some(id);
                    ids.push(id);
                    item
                })
                .collect();
            s.put_contents(batch).await.unwrap();
            // And one more in a later batch.
            let (id5, _) = s.put_contents(vec![(Bytes::from(version(5)), prev)]).await.unwrap()[0];
            ids.push(id5);
            s.inner.decoded.invalidate_all();
            for (n, id) in ids.iter().enumerate() {
                assert_eq!(s.get_content(id).await.unwrap().unwrap(), version(n));
            }
            // Each version is a delta against its predecessor.
            let raw = s.backend_get(ObjectKind::Content, ids[3].as_bytes()).await.unwrap().unwrap();
            let h = Header::parse(&raw).unwrap();
            assert_eq!(h.codec, ObjectCodec::Delta);
            assert_eq!(h.depth, 3);
            assert_eq!(&raw[HEADER_LEN..HEADER_LEN + 32], ids[2].as_bytes());
            assert!(raw.len() < 200, "delta is tiny: {}", raw.len());
            // Chains are capped: a long history keeps restarting from whole objects.
            let mut prev = Some(id5);
            for n in 6..40 {
                let (id, _) = s.put_contents(vec![(Bytes::from(version(n)), prev)]).await.unwrap()[0];
                let raw = s.backend_get(ObjectKind::Content, id.as_bytes()).await.unwrap().unwrap();
                assert!(Header::parse(&raw).unwrap().depth <= max_depth(ObjectKind::Content));
                prev = Some(id);
            }
            s.inner.decoded.invalidate_all();
            assert_eq!(s.get_content(&prev.unwrap()).await.unwrap().unwrap(), version(39));
        }
    }

    #[tokio::test]
    async fn chunked_content() {
        for (_d, s) in stores() {
            // Pseudo-random so chunk boundaries are content-defined.
            let mut x: u64 = 0x9E3779B97F4A7C15;
            let data: Vec<u8> = (0..(11 << 20))
                .map(|_| {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    (x >> 24) as u8
                })
                .collect();
            let (id, meta) = s.put_content(&data).await.unwrap();
            assert_eq!(meta.size, data.len() as u64);
            s.inner.decoded.invalidate_all();
            assert_eq!(s.get_content(&id).await.unwrap().unwrap(), data);
            match s.content_layout(&id).await.unwrap().unwrap() {
                ContentLayout::Chunked { size, chunks } => {
                    assert_eq!(size, data.len() as u64);
                    assert!(chunks.len() >= 3);
                    assert_eq!(chunks.iter().map(|c| c.1 as u64).sum::<u64>(), size);
                }
                ContentLayout::Whole(_) => panic!("expected chunked"),
            }
            // Streaming in odd-sized pieces gives the same chunks.
            let mut sink = s.chunk_sink();
            for piece in data.chunks(777_777) {
                sink.push(piece).await.unwrap();
            }
            let id2 = ContentId::hash(b"other id, same bytes");
            sink.finish(id2, data.len() as u64).await.unwrap();
            let (a, b) = match (s.content_layout(&id).await.unwrap().unwrap(), s.content_layout(&id2).await.unwrap().unwrap()) {
                (ContentLayout::Chunked { chunks: a, .. }, ContentLayout::Chunked { chunks: b, .. }) => (a, b),
                _ => panic!(),
            };
            assert_eq!(a, b);
        }
    }

    /// Small files that share vocabulary, like source files in one repo.
    fn sample_file(i: usize) -> Vec<u8> {
        format!(
            "// Copyright flux authors\nuse std::collections::HashMap;\n\npub fn handler_{i}(req: &Request) -> Result<Response, Error> {{\n    let id = req.param(\"id\")?;\n    tracing::info!(id, \"handling request {i}\");\n    Ok(Response::json(&lookup(id)?))\n}}\n"
        )
        .into_bytes()
    }

    #[tokio::test]
    async fn trained_dictionary_compresses_small_objects() {
        for (dir, s) in stores() {
            let samples: Vec<Vec<u8>> = (0..400).map(sample_file).collect();
            let raw = crate::dict::train(&samples, 16 << 10).unwrap();
            let before = encode(ObjectKind::Content, &sample_file(9999), None, None).unwrap().0.len();
            let id = s.install_dict(ObjectKind::Content, raw).await.unwrap();
            assert_eq!(id, 1);
            assert_eq!(s.list_dicts().await.unwrap(), vec![(1, ObjectKind::Content, s.dict(1).await.unwrap().raw.len())]);
            let data = sample_file(9999);
            let (cid, _) = s.put_content(&data).await.unwrap();
            let stored = s.backend_get(ObjectKind::Content, cid.as_bytes()).await.unwrap().unwrap();
            let h = Header::parse(&stored).unwrap();
            assert_eq!((h.codec, h.depth), (ObjectCodec::Dict, 1));
            assert!(stored.len() * 2 < before, "dictionary should at least halve a small file: {} vs {before}", stored.len());
            assert_eq!(s.get_content(&cid).await.unwrap().unwrap(), data);
            // A fresh process loads the dictionary on demand.
            if let Ok(url) = url::Url::from_directory_path(dir.path())
                && matches!(s.inner.backend, Backend::Segments(_))
            {
                s.sync().await.unwrap();
                drop(s);
                let reopened = BlobStore::open(url.as_str()).unwrap();
                assert_eq!(reopened.get_content(&cid).await.unwrap().unwrap(), data);
                // Ids are never reused.
                assert_eq!(
                    reopened.install_dict(ObjectKind::Tree, crate::dict::train(&samples, 8 << 10).unwrap()).await.unwrap(),
                    2
                );
            }
        }
    }
}
