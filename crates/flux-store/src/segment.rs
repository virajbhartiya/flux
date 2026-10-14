//! Append-only segment store for the local all-in-one layout (SPEC §5.2).
//!
//! Objects are appended to `seg-NNNNNNNN.pack` files. Once a segment reaches
//! its size limit it is sealed: a sorted index (`.idx`, 256-way fanout like a
//! git pack index) is written next to it and both files are memory-mapped.
//! The active (unsealed) segment is indexed in memory and rebuilt by scanning
//! on open; a torn tail is detected by the per-record CRC and truncated.
//!
//! Record: `magic u32 | payload_len u32 | kind u8 | id [32] | payload | crc32`
//! (little-endian; the CRC covers kind, id and payload).
//!
//! Index: `"FXI1" | count u32 | fanout [u32; 256] | entries | crc32`, where an
//! entry is `id [32] | offset u64 | len u32 | kind u8 | pad [3]` (48 bytes).

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use memmap2::Mmap;

const REC_MAGIC: u32 = u32::from_le_bytes(*b"FXR1");
const REC_HEADER: usize = 4 + 4 + 1 + 32;
const REC_TRAILER: usize = 4;
const IDX_MAGIC: &[u8; 4] = b"FXI1";
const IDX_HEADER: usize = 8 + 256 * 4;
const IDX_ENTRY: usize = 48;
/// Default size at which the active segment is sealed.
pub const DEFAULT_SEAL_BYTES: u64 = 1 << 30;

#[derive(Debug, Clone, Copy)]
struct Loc {
    /// Offset of the payload (not the record header).
    off: u64,
    len: u32,
    kind: u8,
}

struct Sealed {
    pack: Mmap,
    idx: Mmap,
}

impl Sealed {
    fn count(&self) -> usize {
        u32::from_le_bytes(self.idx[4..8].try_into().unwrap()) as usize
    }

    fn fanout(&self, b: usize) -> usize {
        u32::from_le_bytes(self.idx[8 + b * 4..12 + b * 4].try_into().unwrap()) as usize
    }

    fn entry_id(&self, i: usize) -> &[u8] {
        let o = IDX_HEADER + i * IDX_ENTRY;
        &self.idx[o..o + 32]
    }

    fn lookup(&self, id: &[u8; 32]) -> Option<Loc> {
        let b = id[0] as usize;
        let mut lo = if b == 0 { 0 } else { self.fanout(b - 1) };
        let mut hi = self.fanout(b);
        while lo < hi {
            let mid = (lo + hi) / 2;
            match self.entry_id(mid).cmp(id.as_slice()) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => {
                    let o = IDX_HEADER + mid * IDX_ENTRY + 32;
                    let e = &self.idx[o..o + 16];
                    return Some(Loc {
                        off: u64::from_le_bytes(e[0..8].try_into().unwrap()),
                        len: u32::from_le_bytes(e[8..12].try_into().unwrap()),
                        kind: e[12],
                    });
                }
            }
        }
        None
    }
}

struct State {
    /// Oldest first.
    sealed: Vec<Arc<Sealed>>,
    active_seq: u32,
    active_file: Arc<File>,
    active: HashMap<[u8; 32], Loc>,
}

struct Writer {
    seq: u32,
    file: File,
    len: u64,
    synced: u64,
}

pub struct SegmentStore {
    dir: PathBuf,
    seal_bytes: u64,
    state: RwLock<State>,
    writer: Mutex<Writer>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SegmentStats {
    pub segments: usize,
    pub objects: usize,
    pub bytes: u64,
}

fn pack_path(dir: &Path, seq: u32) -> PathBuf {
    dir.join(format!("seg-{seq:08}.pack"))
}

fn idx_path(dir: &Path, seq: u32) -> PathBuf {
    dir.join(format!("seg-{seq:08}.idx"))
}

fn crc(kind: u8, id: &[u8; 32], payload: &[u8]) -> u32 {
    let mut h = crc32fast::Hasher::new();
    h.update(&[kind]);
    h.update(id);
    h.update(payload);
    h.finalize()
}

#[cfg(unix)]
fn read_at(f: &File, buf: &mut [u8], off: u64) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt;
    f.read_exact_at(buf, off)
}

#[cfg(windows)]
fn read_at(f: &File, mut buf: &mut [u8], mut off: u64) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        let n = f.seek_read(buf, off)?;
        if n == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        buf = &mut buf[n..];
        off += n as u64;
    }
    Ok(())
}

/// Scan a pack, returning its index and the length of the valid prefix.
fn scan(path: &Path) -> std::io::Result<(HashMap<[u8; 32], Loc>, u64)> {
    let mut map = HashMap::new();
    let mut r = BufReader::with_capacity(4 << 20, File::open(path)?);
    let mut pos = 0u64;
    let mut header = [0u8; REC_HEADER];
    let mut payload = Vec::new();
    loop {
        match r.read_exact(&mut header) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e),
        }
        if u32::from_le_bytes(header[0..4].try_into().unwrap()) != REC_MAGIC {
            break;
        }
        let len = u32::from_le_bytes(header[4..8].try_into().unwrap());
        let kind = header[8];
        let id: [u8; 32] = header[9..41].try_into().unwrap();
        payload.resize(len as usize, 0);
        let mut trailer = [0u8; REC_TRAILER];
        if r.read_exact(&mut payload).is_err() || r.read_exact(&mut trailer).is_err() {
            break;
        }
        if u32::from_le_bytes(trailer) != crc(kind, &id, &payload) {
            break;
        }
        map.insert(id, Loc { off: pos + REC_HEADER as u64, len, kind });
        pos += (REC_HEADER + len as usize + REC_TRAILER) as u64;
    }
    Ok((map, pos))
}

fn write_index(dir: &Path, seq: u32, map: &HashMap<[u8; 32], Loc>) -> std::io::Result<()> {
    let mut entries: Vec<(&[u8; 32], &Loc)> = map.iter().collect();
    entries.sort_unstable_by(|a, b| a.0.cmp(b.0));
    let mut out = Vec::with_capacity(IDX_HEADER + entries.len() * IDX_ENTRY + 4);
    out.extend_from_slice(IDX_MAGIC);
    out.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    let mut fanout = [0u32; 256];
    for (id, _) in &entries {
        fanout[id[0] as usize] += 1;
    }
    let mut acc = 0u32;
    for f in fanout.iter_mut() {
        acc += *f;
        *f = acc;
    }
    for f in fanout {
        out.extend_from_slice(&f.to_le_bytes());
    }
    for (id, loc) in entries {
        out.extend_from_slice(id);
        out.extend_from_slice(&loc.off.to_le_bytes());
        out.extend_from_slice(&loc.len.to_le_bytes());
        out.extend_from_slice(&[loc.kind, 0, 0, 0]);
    }
    let sum = crc32fast::hash(&out);
    out.extend_from_slice(&sum.to_le_bytes());
    let tmp = dir.join(format!("seg-{seq:08}.idx.tmp"));
    let mut f = File::create(&tmp)?;
    f.write_all(&out)?;
    f.sync_all()?;
    std::fs::rename(&tmp, idx_path(dir, seq))
}

fn open_sealed(dir: &Path, seq: u32) -> std::io::Result<Option<Sealed>> {
    let idx_file = match File::open(idx_path(dir, seq)) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    // SAFETY: sealed segments and their indexes are immutable once renamed
    // into place; flux never modifies or truncates them.
    let idx = unsafe { Mmap::map(&idx_file)? };
    let valid = idx.len() >= IDX_HEADER + 4 && &idx[0..4] == IDX_MAGIC && {
        let n = u32::from_le_bytes(idx[4..8].try_into().unwrap()) as usize;
        idx.len() == IDX_HEADER + n * IDX_ENTRY + 4
            && crc32fast::hash(&idx[..idx.len() - 4]) == u32::from_le_bytes(idx[idx.len() - 4..].try_into().unwrap())
    };
    if !valid {
        tracing::warn!(seq, "ignoring invalid segment index; it will be rebuilt");
        return Ok(None);
    }
    let pack = unsafe { Mmap::map(&File::open(pack_path(dir, seq))?)? };
    #[cfg(unix)]
    let _ = pack.advise(memmap2::Advice::Random);
    Ok(Some(Sealed { pack, idx }))
}

impl SegmentStore {
    pub fn open(dir: impl Into<PathBuf>) -> std::io::Result<Self> {
        Self::open_with(dir, DEFAULT_SEAL_BYTES)
    }

    pub fn open_with(dir: impl Into<PathBuf>, seal_bytes: u64) -> std::io::Result<Self> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;
        let mut seqs: Vec<u32> = std::fs::read_dir(&dir)?
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let name = e.file_name().into_string().ok()?;
                name.strip_prefix("seg-")?.strip_suffix(".pack")?.parse().ok()
            })
            .collect();
        seqs.sort_unstable();
        let mut sealed = Vec::new();
        let last = seqs.last().copied();
        let mut active: Option<(u32, HashMap<[u8; 32], Loc>, u64)> = None;
        for &seq in &seqs {
            if let Some(s) = open_sealed(&dir, seq)? {
                sealed.push(Arc::new(s));
                continue;
            }
            let (map, valid) = scan(&pack_path(&dir, seq))?;
            let len = std::fs::metadata(pack_path(&dir, seq))?.len();
            if valid < len {
                tracing::warn!(seq, valid, len, "truncating torn segment tail");
                OpenOptions::new().write(true).open(pack_path(&dir, seq))?.set_len(valid)?;
            }
            if Some(seq) == last {
                active = Some((seq, map, valid));
            } else {
                // Crashed while sealing: finish the job.
                write_index(&dir, seq, &map)?;
                sealed.push(Arc::new(open_sealed(&dir, seq)?.expect("index just written")));
            }
        }
        let (seq, map, len) = match active {
            Some(a) => a,
            None => (last.map(|s| s + 1).unwrap_or(1), HashMap::new(), 0),
        };
        let file = OpenOptions::new().create(true).append(true).read(true).open(pack_path(&dir, seq))?;
        let reader = Arc::new(file.try_clone()?);
        Ok(Self {
            dir,
            seal_bytes,
            state: RwLock::new(State { sealed, active_seq: seq, active_file: reader, active: map }),
            writer: Mutex::new(Writer { seq, file, len, synced: len }),
        })
    }

    fn locate(&self, id: &[u8; 32]) -> Option<(Loc, Option<Arc<Sealed>>, Option<Arc<File>>)> {
        let st = self.state.read().unwrap();
        if let Some(l) = st.active.get(id) {
            return Some((*l, None, Some(st.active_file.clone())));
        }
        for s in st.sealed.iter().rev() {
            if let Some(l) = s.lookup(id) {
                return Some((l, Some(s.clone()), None));
            }
        }
        None
    }

    pub fn contains(&self, id: &[u8; 32]) -> bool {
        self.locate(id).is_some()
    }

    /// Read an object's payload; `None` if absent. Returns `(kind, payload)`.
    pub fn get(&self, id: &[u8; 32]) -> std::io::Result<Option<(u8, Vec<u8>)>> {
        let Some((loc, sealed, file)) = self.locate(id) else {
            return Ok(None);
        };
        let mut buf = vec![0u8; loc.len as usize];
        match (sealed, file) {
            (Some(s), _) => {
                let start = loc.off as usize;
                let end = start + loc.len as usize;
                if end > s.pack.len() {
                    return Err(std::io::Error::other("segment index points past end of pack"));
                }
                buf.copy_from_slice(&s.pack[start..end]);
            }
            (None, Some(f)) => read_at(&f, &mut buf, loc.off)?,
            _ => unreachable!(),
        }
        Ok(Some((loc.kind, buf)))
    }

    /// Read only the first `n` payload bytes (cheap header peeks).
    pub fn get_prefix(&self, id: &[u8; 32], n: usize) -> std::io::Result<Option<Vec<u8>>> {
        let Some((loc, sealed, file)) = self.locate(id) else {
            return Ok(None);
        };
        let n = n.min(loc.len as usize);
        let mut buf = vec![0u8; n];
        match (sealed, file) {
            (Some(s), _) => buf.copy_from_slice(&s.pack[loc.off as usize..loc.off as usize + n]),
            (None, Some(f)) => read_at(&f, &mut buf, loc.off)?,
            _ => unreachable!(),
        }
        Ok(Some(buf))
    }

    /// Append objects (skipping ids already present). One write syscall per
    /// batch. Not durable until [`SegmentStore::sync`].
    pub fn put_batch(&self, items: &[(u8, [u8; 32], &[u8])]) -> std::io::Result<()> {
        let mut w = self.writer.lock().unwrap();
        let mut buf = Vec::new();
        let mut locs = Vec::with_capacity(items.len());
        let mut batch_ids = std::collections::HashSet::new();
        for (kind, id, payload) in items {
            if !batch_ids.insert(*id) || self.contains(id) {
                continue;
            }
            let len = u32::try_from(payload.len()).map_err(|_| std::io::Error::other("object too large for segment"))?;
            let rec_start = w.len + buf.len() as u64;
            buf.extend_from_slice(&REC_MAGIC.to_le_bytes());
            buf.extend_from_slice(&len.to_le_bytes());
            buf.push(*kind);
            buf.extend_from_slice(id);
            buf.extend_from_slice(payload);
            buf.extend_from_slice(&crc(*kind, id, payload).to_le_bytes());
            locs.push((*id, Loc { off: rec_start + REC_HEADER as u64, len, kind: *kind }));
        }
        if buf.is_empty() {
            return Ok(());
        }
        w.file.write_all(&buf)?;
        w.len += buf.len() as u64;
        {
            let mut st = self.state.write().unwrap();
            st.active.extend(locs);
        }
        if w.len >= self.seal_bytes {
            self.seal(&mut w)?;
        }
        Ok(())
    }

    fn seal(&self, w: &mut Writer) -> std::io::Result<()> {
        w.file.sync_data()?;
        let map = self.state.read().unwrap().active.clone();
        write_index(&self.dir, w.seq, &map)?;
        let sealed = Arc::new(open_sealed(&self.dir, w.seq)?.expect("index just written"));
        let next = w.seq + 1;
        let file = OpenOptions::new().create(true).append(true).read(true).open(pack_path(&self.dir, next))?;
        let reader = Arc::new(file.try_clone()?);
        {
            let mut st = self.state.write().unwrap();
            st.sealed.push(sealed);
            st.active.clear();
            st.active_seq = next;
            st.active_file = reader;
        }
        *w = Writer { seq: next, file, len: 0, synced: 0 };
        tracing::debug!(seq = next - 1, "sealed segment");
        Ok(())
    }

    /// Make everything written so far durable (group commit: a no-op when
    /// another caller already synced past our writes).
    pub fn sync(&self) -> std::io::Result<()> {
        let mut w = self.writer.lock().unwrap();
        if w.synced < w.len {
            w.file.sync_data()?;
            w.synced = w.len;
        }
        Ok(())
    }

    pub fn stats(&self) -> SegmentStats {
        // Lock order: writer, then state (same as put_batch).
        let w = self.writer.lock().unwrap();
        let st = self.state.read().unwrap();
        SegmentStats {
            segments: st.sealed.len() + 1,
            objects: st.active.len() + st.sealed.iter().map(|s| s.count()).sum::<usize>(),
            bytes: w.len + st.sealed.iter().map(|s| s.pack.len() as u64).sum::<u64>(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u32) -> [u8; 32] {
        *blake3::hash(&n.to_le_bytes()).as_bytes()
    }

    #[test]
    fn put_get_seal_reopen() {
        let dir = tempfile::tempdir().unwrap();
        {
            let s = SegmentStore::open_with(dir.path(), 4096).unwrap();
            for n in 0..200u32 {
                let data = vec![n as u8; (n % 97) as usize + 1];
                s.put_batch(&[(1, id(n), &data)]).unwrap();
            }
            s.sync().unwrap();
            assert!(s.stats().segments > 1, "should have sealed at 4 KiB");
            for n in 0..200u32 {
                let (kind, data) = s.get(&id(n)).unwrap().unwrap();
                assert_eq!(kind, 1);
                assert_eq!(data, vec![n as u8; (n % 97) as usize + 1]);
            }
            assert!(s.get(&id(9999)).unwrap().is_none());
        }
        let s = SegmentStore::open_with(dir.path(), 4096).unwrap();
        assert_eq!(s.stats().objects, 200);
        assert_eq!(s.get(&id(150)).unwrap().unwrap().1, vec![150u8; 150 % 97 + 1]);
        assert_eq!(s.get_prefix(&id(150), 3).unwrap().unwrap(), vec![150u8; 3]);
    }

    #[test]
    fn torn_tail_is_truncated() {
        let dir = tempfile::tempdir().unwrap();
        {
            let s = SegmentStore::open(dir.path()).unwrap();
            s.put_batch(&[(1, id(1), b"one"), (1, id(2), b"two")]).unwrap();
            s.sync().unwrap();
        }
        // Simulate a torn write: append half a record.
        let p = pack_path(dir.path(), 1);
        let mut f = OpenOptions::new().append(true).open(&p).unwrap();
        f.write_all(&REC_MAGIC.to_le_bytes()).unwrap();
        f.write_all(&[9, 0, 0]).unwrap();
        drop(f);
        let s = SegmentStore::open(dir.path()).unwrap();
        assert_eq!(s.get(&id(2)).unwrap().unwrap().1, b"two");
        s.put_batch(&[(1, id(3), b"three")]).unwrap();
        drop(s);
        let s = SegmentStore::open(dir.path()).unwrap();
        assert_eq!(s.get(&id(3)).unwrap().unwrap().1, b"three");
        assert_eq!(s.stats().objects, 3);
    }

    #[test]
    fn dedups_within_and_across_batches() {
        let dir = tempfile::tempdir().unwrap();
        let s = SegmentStore::open(dir.path()).unwrap();
        s.put_batch(&[(1, id(1), b"x"), (1, id(1), b"x")]).unwrap();
        s.put_batch(&[(1, id(1), b"x")]).unwrap();
        assert_eq!(s.stats().objects, 1);
    }
}
