//! git packfiles: writing (for clone/fetch) and reading (for push), plus
//! pkt-line framing and git's binary delta format.

use std::collections::HashMap;
use std::io::{Read, Write};

use flate2::Compression;
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use sha1::Digest as _;

use super::objects::{Kind, Sha, hash_object};

// ───────────── pkt-line ─────────────

pub fn pkt(out: &mut Vec<u8>, data: &[u8]) {
    out.extend_from_slice(format!("{:04x}", data.len() + 4).as_bytes());
    out.extend_from_slice(data);
}

pub fn pkt_str(out: &mut Vec<u8>, s: &str) {
    pkt(out, s.as_bytes());
}

pub fn flush(out: &mut Vec<u8>) {
    out.extend_from_slice(b"0000");
}

/// Split a request body into pkt-lines; `None` marks a flush packet.
/// Returns the lines and the offset where pkt framing stopped (a pack may
/// follow in receive-pack bodies).
pub fn read_pkts(data: &[u8]) -> Result<(Vec<Option<Vec<u8>>>, usize), String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 4 <= data.len() {
        if data[i..].starts_with(b"PACK") {
            break;
        }
        let len = usize::from_str_radix(std::str::from_utf8(&data[i..i + 4]).map_err(|_| "bad pkt length")?, 16)
            .map_err(|_| "bad pkt length")?;
        if len == 0 {
            out.push(None);
            i += 4;
            continue;
        }
        if len < 4 || i + len > data.len() {
            return Err("truncated pkt-line".into());
        }
        out.push(Some(data[i + 4..i + len].to_vec()));
        i += len;
    }
    Ok((out, i))
}

/// Side-band-64k framing for channel 1 (data), 2 (progress), 3 (error).
pub fn sideband(out: &mut Vec<u8>, channel: u8, data: &[u8]) {
    for chunk in data.chunks(65515) {
        out.extend_from_slice(format!("{:04x}", chunk.len() + 5).as_bytes());
        out.push(channel);
        out.extend_from_slice(chunk);
    }
}

// ───────────── delta format ─────────────

fn put_size(out: &mut Vec<u8>, mut n: usize) {
    loop {
        let mut b = (n & 0x7f) as u8;
        n >>= 7;
        if n > 0 {
            b |= 0x80;
        }
        out.push(b);
        if n == 0 {
            break;
        }
    }
}

const BLOCK: usize = 16;

/// Encode `target` as a git delta against `base`; `None` if it doesn't pay.
pub fn make_delta(base: &[u8], target: &[u8]) -> Option<Vec<u8>> {
    if base.len() < BLOCK || target.len() < BLOCK || base.len() > (16 << 20) || target.len() > (16 << 20) {
        return None;
    }
    let hash = |w: &[u8]| -> u64 {
        let mut h: u64 = 0xcbf29ce484222325;
        for &b in w {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        h
    };
    let mut index: HashMap<u64, usize> = HashMap::with_capacity(base.len() / BLOCK + 1);
    let mut i = 0;
    while i + BLOCK <= base.len() {
        index.entry(hash(&base[i..i + BLOCK])).or_insert(i);
        i += BLOCK;
    }
    let mut out = Vec::with_capacity(target.len() / 4 + 16);
    put_size(&mut out, base.len());
    put_size(&mut out, target.len());
    let mut pending: Vec<u8> = Vec::new();
    let flush_insert = |out: &mut Vec<u8>, pending: &mut Vec<u8>| {
        for chunk in pending.chunks(127) {
            out.push(chunk.len() as u8);
            out.extend_from_slice(chunk);
        }
        pending.clear();
    };
    let mut t = 0;
    while t < target.len() {
        let m = if t + BLOCK <= target.len() {
            index.get(&hash(&target[t..t + BLOCK])).copied().filter(|&b| base[b..b + BLOCK] == target[t..t + BLOCK])
        } else {
            None
        };
        let Some(mut b) = m else {
            pending.push(target[t]);
            t += 1;
            continue;
        };
        // Extend backwards into pending inserts, then forwards.
        let mut start_t = t;
        while start_t > 0 && b > 0 && !pending.is_empty() && base[b - 1] == target[start_t - 1] {
            pending.pop();
            start_t -= 1;
            b -= 1;
        }
        let mut len = 0;
        while start_t + len < target.len() && b + len < base.len() && base[b + len] == target[start_t + len] && len < 0xffffff {
            len += 1;
        }
        flush_insert(&mut out, &mut pending);
        // Copy op: offset and size bytes, present bits in the opcode.
        let mut op = 0x80u8;
        let mut args = Vec::with_capacity(7);
        for k in 0..4 {
            let byte = ((b >> (8 * k)) & 0xff) as u8;
            if byte != 0 {
                op |= 1 << k;
                args.push(byte);
            }
        }
        for k in 0..3 {
            let byte = ((len >> (8 * k)) & 0xff) as u8;
            if byte != 0 {
                op |= 0x10 << k;
                args.push(byte);
            }
        }
        out.push(op);
        out.extend_from_slice(&args);
        t = start_t + len;
        if out.len() > target.len() / 2 {
            return None;
        }
    }
    flush_insert(&mut out, &mut pending);
    (out.len() < target.len() / 2).then_some(out)
}

fn get_size(d: &[u8], i: &mut usize) -> Result<usize, String> {
    let mut n = 0usize;
    let mut shift = 0;
    loop {
        let b = *d.get(*i).ok_or("truncated delta")?;
        *i += 1;
        n |= ((b & 0x7f) as usize) << shift;
        shift += 7;
        if b & 0x80 == 0 {
            return Ok(n);
        }
    }
}

pub fn apply_delta(base: &[u8], delta: &[u8]) -> Result<Vec<u8>, String> {
    let mut i = 0;
    let bsize = get_size(delta, &mut i)?;
    if bsize != base.len() {
        return Err("delta base size mismatch".into());
    }
    let tsize = get_size(delta, &mut i)?;
    let mut out = Vec::with_capacity(tsize);
    while i < delta.len() {
        let op = delta[i];
        i += 1;
        if op & 0x80 != 0 {
            let mut off = 0usize;
            let mut len = 0usize;
            for k in 0..4 {
                if op & (1 << k) != 0 {
                    off |= (*delta.get(i).ok_or("truncated delta")? as usize) << (8 * k);
                    i += 1;
                }
            }
            for k in 0..3 {
                if op & (0x10 << k) != 0 {
                    len |= (*delta.get(i).ok_or("truncated delta")? as usize) << (8 * k);
                    i += 1;
                }
            }
            if len == 0 {
                len = 0x10000;
            }
            out.extend_from_slice(base.get(off..off + len).ok_or("delta copy out of range")?);
        } else if op > 0 {
            let n = op as usize;
            out.extend_from_slice(delta.get(i..i + n).ok_or("truncated delta insert")?);
            i += n;
        } else {
            return Err("reserved delta opcode".into());
        }
    }
    if out.len() != tsize {
        return Err("delta result size mismatch".into());
    }
    Ok(out)
}

// ───────────── writing ─────────────

/// Streams a version-2 packfile. The object count is fixed up front.
pub struct PackWriter {
    hasher: sha1::Sha1,
    /// Offsets of written objects (for OFS_DELTA).
    offsets: HashMap<Sha, u64>,
    pos: u64,
}

fn type_size_header(ty: u8, size: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(10);
    let mut b = (ty << 4) | (size & 0x0f) as u8;
    let mut rest = size >> 4;
    if rest > 0 {
        b |= 0x80;
    }
    out.push(b);
    while rest > 0 {
        let mut c = (rest & 0x7f) as u8;
        rest >>= 7;
        if rest > 0 {
            c |= 0x80;
        }
        out.push(c);
    }
    out
}

fn deflate(data: &[u8]) -> Vec<u8> {
    let mut e = ZlibEncoder::new(Vec::with_capacity(data.len() / 2 + 16), Compression::fast());
    e.write_all(data).expect("in-memory write");
    e.finish().expect("in-memory finish")
}

/// An object compressed for a pack, independent of where it lands, so
/// encoding (delta search and deflate) can run in parallel.
pub struct Encoded {
    kind: Kind,
    sha: Sha,
    /// Uncompressed size of the object or delta.
    size: usize,
    body: Vec<u8>,
    delta_base: Option<Sha>,
}

/// Compress `data`, as a delta against `base` when that is smaller.
pub fn encode(kind: Kind, sha: Sha, data: &[u8], base: Option<(&Sha, &[u8])>) -> Encoded {
    if let Some((bsha, bdata)) = base
        && let Some(delta) = make_delta(bdata, data)
    {
        return Encoded { kind, sha, size: delta.len(), body: deflate(&delta), delta_base: Some(*bsha) };
    }
    Encoded { kind, sha, size: data.len(), body: deflate(data), delta_base: None }
}

impl PackWriter {
    pub fn new(count: u32) -> (Self, Vec<u8>) {
        let mut head = Vec::with_capacity(12);
        head.extend_from_slice(b"PACK");
        head.extend_from_slice(&2u32.to_be_bytes());
        head.extend_from_slice(&count.to_be_bytes());
        let mut w = Self { hasher: sha1::Sha1::new(), offsets: HashMap::new(), pos: 0 };
        w.hasher.update(&head);
        w.pos = head.len() as u64;
        (w, head)
    }

    fn emit(&mut self, bytes: Vec<u8>) -> Vec<u8> {
        self.hasher.update(&bytes);
        self.pos += bytes.len() as u64;
        bytes
    }

    /// Append objects already encoded as a pack body (a cached pack without
    /// its header and trailer). Its offsets are not indexed, so later
    /// objects never delta against it.
    pub fn raw(&mut self, body: &[u8]) {
        self.hasher.update(body);
        self.pos += body.len() as u64;
    }

    /// Split a complete pack into `(object count, body)`.
    pub fn split(pack: &[u8]) -> Option<(u32, &[u8])> {
        if pack.len() < 32 || &pack[..4] != b"PACK" {
            return None;
        }
        let count = u32::from_be_bytes(pack[8..12].try_into().ok()?);
        Some((count, &pack[12..pack.len() - 20]))
    }

    /// Append an encoded object. A delta's base must already be in this
    /// pack (OFS_DELTA) unless `thin` allows naming one the client has
    /// (REF_DELTA).
    pub fn place(&mut self, e: Encoded, thin: bool) -> Result<Vec<u8>, String> {
        let start = self.pos;
        let mut out = Vec::with_capacity(e.body.len() + 32);
        match e.delta_base {
            Some(bsha) => match self.offsets.get(&bsha) {
                Some(&boff) => {
                    out.extend(type_size_header(6, e.size));
                    // OFS_DELTA: offset encoded big-endian base-128 with +1 bias.
                    let mut off = start - boff;
                    let mut enc = vec![(off & 0x7f) as u8];
                    off >>= 7;
                    while off > 0 {
                        off -= 1;
                        enc.push(0x80 | (off & 0x7f) as u8);
                        off >>= 7;
                    }
                    enc.reverse();
                    out.extend_from_slice(&enc);
                }
                None if thin => {
                    out.extend(type_size_header(7, e.size));
                    out.extend_from_slice(&bsha);
                }
                None => return Err(format!("delta base {} is not in the pack", super::objects::hex(&bsha))),
            },
            None => out.extend(type_size_header(e.kind as u8, e.size)),
        }
        out.extend_from_slice(&e.body);
        self.offsets.insert(e.sha, start);
        Ok(self.emit(out))
    }

    /// Encode and append in one step (tests and small packs).
    pub fn object(&mut self, kind: Kind, sha: Sha, data: &[u8], base: Option<(&Sha, &[u8])>) -> Vec<u8> {
        let base = base.filter(|(b, _)| self.offsets.contains_key(*b));
        self.place(encode(kind, sha, data, base), false).expect("base checked")
    }

    pub fn finish(self) -> Vec<u8> {
        self.hasher.finalize().to_vec()
    }
}

// ───────────── reading ─────────────

#[derive(Debug, Clone)]
pub struct PackObject {
    pub kind: Kind,
    pub sha: Sha,
    pub data: Vec<u8>,
}

fn inflate_at(data: &[u8], size: usize) -> Result<(Vec<u8>, usize), String> {
    let mut d = ZlibDecoder::new(data);
    let mut out = Vec::with_capacity(size);
    d.read_to_end(&mut out).map_err(|e| format!("zlib: {e}"))?;
    if out.len() != size {
        return Err("object size mismatch".into());
    }
    Ok((out, d.total_in() as usize))
}

/// Parse a packfile, resolving OFS/REF deltas (REF bases may come from
/// `external`, for thin packs). Verifies the trailing checksum.
pub fn read_pack(pack: &[u8], mut external: impl FnMut(&Sha) -> Option<(Kind, Vec<u8>)>) -> Result<Vec<PackObject>, String> {
    if pack.len() < 32 || &pack[0..4] != b"PACK" {
        return Err("not a packfile".into());
    }
    let ver = u32::from_be_bytes(pack[4..8].try_into().unwrap());
    if ver != 2 && ver != 3 {
        return Err(format!("unsupported pack version {ver}"));
    }
    let count = u32::from_be_bytes(pack[8..12].try_into().unwrap()) as usize;
    let body = &pack[..pack.len() - 20];
    let sum: [u8; 20] = sha1::Sha1::digest(body).into();
    if sum != pack[pack.len() - 20..] {
        return Err("pack checksum mismatch".into());
    }
    enum Raw {
        Full(Kind, Vec<u8>),
        Ofs(u64, Vec<u8>),
        Ref(Sha, Vec<u8>),
    }
    let mut raws: Vec<(u64, Raw)> = Vec::with_capacity(count);
    let mut i = 12usize;
    for _ in 0..count {
        let start = i as u64;
        let mut c = *body.get(i).ok_or("truncated pack")?;
        i += 1;
        let ty = (c >> 4) & 7;
        let mut size = (c & 0x0f) as usize;
        let mut shift = 4;
        while c & 0x80 != 0 {
            c = *body.get(i).ok_or("truncated pack")?;
            i += 1;
            size |= ((c & 0x7f) as usize) << shift;
            shift += 7;
        }
        let raw = match ty {
            1..=4 => {
                let kind = [Kind::Commit, Kind::Tree, Kind::Blob, Kind::Tag][(ty - 1) as usize];
                let (data, used) = inflate_at(&body[i..], size)?;
                i += used;
                Raw::Full(kind, data)
            }
            6 => {
                let mut c = *body.get(i).ok_or("truncated pack")?;
                i += 1;
                let mut off = (c & 0x7f) as u64;
                while c & 0x80 != 0 {
                    c = *body.get(i).ok_or("truncated pack")?;
                    i += 1;
                    off = ((off + 1) << 7) | (c & 0x7f) as u64;
                }
                let (data, used) = inflate_at(&body[i..], size)?;
                i += used;
                Raw::Ofs(start.checked_sub(off).ok_or("bad ofs delta")?, data)
            }
            7 => {
                let base: Sha = body.get(i..i + 20).ok_or("truncated pack")?.try_into().unwrap();
                i += 20;
                let (data, used) = inflate_at(&body[i..], size)?;
                i += used;
                Raw::Ref(base, data)
            }
            t => return Err(format!("bad object type {t}")),
        };
        raws.push((start, raw));
    }
    // Resolve deltas (bases may appear after their deltas for REF_DELTA).
    let by_offset: HashMap<u64, usize> = raws.iter().enumerate().map(|(k, (o, _))| (*o, k)).collect();
    let mut done: Vec<Option<(Kind, Vec<u8>, Sha)>> = vec![None; raws.len()];
    let mut by_sha: HashMap<Sha, usize> = HashMap::new();
    for (k, (_, r)) in raws.iter().enumerate() {
        if let Raw::Full(kind, data) = r {
            let sha = hash_object(*kind, data);
            by_sha.insert(sha, k);
            done[k] = Some((*kind, data.clone(), sha));
        }
    }
    loop {
        let mut progress = false;
        let mut pending = false;
        for k in 0..raws.len() {
            if done[k].is_some() {
                continue;
            }
            let base: Option<(Kind, Vec<u8>)> = match &raws[k].1 {
                Raw::Ofs(off, _) => {
                    let j = *by_offset.get(off).ok_or("ofs delta base not found")?;
                    done[j].as_ref().map(|(kd, d, _)| (*kd, d.clone()))
                }
                Raw::Ref(sha, _) => match by_sha.get(sha) {
                    Some(&j) => done[j].as_ref().map(|(kd, d, _)| (*kd, d.clone())),
                    None => external(sha),
                },
                Raw::Full(..) => unreachable!(),
            };
            let Some((kind, bdata)) = base else {
                pending = true;
                continue;
            };
            let delta = match &raws[k].1 {
                Raw::Ofs(_, d) | Raw::Ref(_, d) => d,
                Raw::Full(..) => unreachable!(),
            };
            let data = apply_delta(&bdata, delta)?;
            let sha = hash_object(kind, &data);
            by_sha.insert(sha, k);
            done[k] = Some((kind, data, sha));
            progress = true;
        }
        if !pending {
            break;
        }
        if !progress {
            return Err("unresolvable delta base (thin pack without a known base)".into());
        }
    }
    Ok(done
        .into_iter()
        .map(|d| {
            let (kind, data, sha) = d.unwrap();
            PackObject { kind, sha, data }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delta_roundtrip() {
        let base: Vec<u8> = (0..4000).flat_map(|i| format!("line {i}\n").into_bytes()).collect();
        let mut target = base.clone();
        target.splice(1000..1010, b"CHANGED!".iter().copied());
        target.extend_from_slice(b"tail\n");
        let d = make_delta(&base, &target).unwrap();
        assert!(d.len() < 200, "{}", d.len());
        assert_eq!(apply_delta(&base, &d).unwrap(), target);
    }

    #[test]
    fn pack_roundtrip_with_deltas() {
        let a: Vec<u8> = (0..2000).flat_map(|i| format!("row {i}\n").into_bytes()).collect();
        let mut b = a.clone();
        b.extend_from_slice(b"more\n");
        let (sa, sb) = (hash_object(Kind::Blob, &a), hash_object(Kind::Blob, &b));
        let (mut w, mut pack) = PackWriter::new(2);
        pack.extend(w.object(Kind::Blob, sa, &a, None));
        pack.extend(w.object(Kind::Blob, sb, &b, Some((&sa, &a))));
        pack.extend(w.finish());
        let objs = read_pack(&pack, |_| None).unwrap();
        assert_eq!(objs.len(), 2);
        assert_eq!(objs[1].sha, sb);
        assert_eq!(objs[1].data, b);
    }

    #[test]
    fn pkt_lines() {
        let mut v = Vec::new();
        pkt_str(&mut v, "want abc\n");
        flush(&mut v);
        let (lines, end) = read_pkts(&v).unwrap();
        assert_eq!(lines, vec![Some(b"want abc\n".to_vec()), None]);
        assert_eq!(end, v.len());
    }
}
