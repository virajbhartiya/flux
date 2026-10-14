//! Minimal deterministic CBOR (RFC 8949 §4.2.1) for the subset flux uses.
//!
//! Supported items: unsigned/negative integers, byte strings, text strings,
//! definite-length arrays, `null`, `true`/`false`. Structures are encoded as
//! positional arrays, so map key ordering never arises. The decoder rejects
//! anything outside that subset as well as any non-shortest-form head, which
//! makes "decode then re-encode" byte-identical by construction.

use crate::error::{Error, Result};

const MT_UINT: u8 = 0;
const MT_NINT: u8 = 1;
const MT_BYTES: u8 = 2;
const MT_TEXT: u8 = 3;
const MT_ARRAY: u8 = 4;

const FALSE: u8 = 0xf4;
const TRUE: u8 = 0xf5;
const NULL: u8 = 0xf6;

/// Hard cap on nesting to bound decoder recursion on hostile input.
const MAX_DEPTH: usize = 32;

#[derive(Default)]
pub struct Encoder {
    buf: Vec<u8>,
}

impl Encoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }

    fn head(&mut self, major: u8, v: u64) {
        let mt = major << 5;
        if v < 24 {
            self.buf.push(mt | v as u8);
        } else if v <= u8::MAX as u64 {
            self.buf.push(mt | 24);
            self.buf.push(v as u8);
        } else if v <= u16::MAX as u64 {
            self.buf.push(mt | 25);
            self.buf.extend_from_slice(&(v as u16).to_be_bytes());
        } else if v <= u32::MAX as u64 {
            self.buf.push(mt | 26);
            self.buf.extend_from_slice(&(v as u32).to_be_bytes());
        } else {
            self.buf.push(mt | 27);
            self.buf.extend_from_slice(&v.to_be_bytes());
        }
    }

    pub fn uint(&mut self, v: u64) -> &mut Self {
        self.head(MT_UINT, v);
        self
    }

    pub fn int(&mut self, v: i64) -> &mut Self {
        if v >= 0 {
            self.head(MT_UINT, v as u64);
        } else {
            // -1 - n encoding; !v == -1 - v for two's complement.
            self.head(MT_NINT, !v as u64);
        }
        self
    }

    pub fn bytes(&mut self, b: &[u8]) -> &mut Self {
        self.head(MT_BYTES, b.len() as u64);
        self.buf.extend_from_slice(b);
        self
    }

    pub fn text(&mut self, s: &str) -> &mut Self {
        self.head(MT_TEXT, s.len() as u64);
        self.buf.extend_from_slice(s.as_bytes());
        self
    }

    pub fn array(&mut self, len: usize) -> &mut Self {
        self.head(MT_ARRAY, len as u64);
        self
    }

    pub fn null(&mut self) -> &mut Self {
        self.buf.push(NULL);
        self
    }

    pub fn bool(&mut self, v: bool) -> &mut Self {
        self.buf.push(if v { TRUE } else { FALSE });
        self
    }
}

pub struct Decoder<'a> {
    data: &'a [u8],
    pos: usize,
    depth: usize,
}

impl<'a> Decoder<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0, depth: 0 }
    }

    fn err<T>(&self, msg: impl Into<String>) -> Result<T> {
        Err(Error::Decode(format!("{} at byte {}", msg.into(), self.pos)))
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.data.len() - self.pos < n {
            return self.err("unexpected end of input");
        }
        let s = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    fn peek(&self) -> Result<u8> {
        match self.data.get(self.pos) {
            Some(b) => Ok(*b),
            None => self.err("unexpected end of input"),
        }
    }

    /// Read a head, enforcing shortest-form encoding.
    fn head(&mut self) -> Result<(u8, u64)> {
        let b = self.take(1)?[0];
        let major = b >> 5;
        let info = b & 0x1f;
        let v = match info {
            0..=23 => info as u64,
            24 => {
                let v = self.take(1)?[0] as u64;
                if v < 24 {
                    return Err(Error::NonCanonical);
                }
                v
            }
            25 => {
                let v = u16::from_be_bytes(self.take(2)?.try_into().unwrap()) as u64;
                if v <= u8::MAX as u64 {
                    return Err(Error::NonCanonical);
                }
                v
            }
            26 => {
                let v = u32::from_be_bytes(self.take(4)?.try_into().unwrap()) as u64;
                if v <= u16::MAX as u64 {
                    return Err(Error::NonCanonical);
                }
                v
            }
            27 => {
                let v = u64::from_be_bytes(self.take(8)?.try_into().unwrap());
                if v <= u32::MAX as u64 {
                    return Err(Error::NonCanonical);
                }
                v
            }
            _ => return self.err("indefinite length or reserved additional info"),
        };
        Ok((major, v))
    }

    fn expect_major(&mut self, want: u8, what: &str) -> Result<u64> {
        let (major, v) = self.head()?;
        if major != want {
            return self.err(format!("expected {what}, got major type {major}"));
        }
        Ok(v)
    }

    pub fn uint(&mut self) -> Result<u64> {
        self.expect_major(MT_UINT, "uint")
    }

    pub fn int(&mut self) -> Result<i64> {
        let (major, v) = self.head()?;
        match major {
            MT_UINT => i64::try_from(v).or_else(|_| self.err("int overflow")),
            MT_NINT => {
                if v > i64::MAX as u64 {
                    return self.err("int overflow");
                }
                Ok(!(v as i64))
            }
            _ => self.err("expected int"),
        }
    }

    pub fn bytes(&mut self) -> Result<&'a [u8]> {
        let len = self.expect_major(MT_BYTES, "bytes")?;
        let len = usize::try_from(len).or_else(|_| self.err("length overflow"))?;
        self.take(len)
    }

    pub fn bytes_fixed<const N: usize>(&mut self) -> Result<[u8; N]> {
        let b = self.bytes()?;
        b.try_into().or_else(|_| self.err(format!("expected {N} bytes, got {}", b.len())))
    }

    pub fn text(&mut self) -> Result<&'a str> {
        let len = self.expect_major(MT_TEXT, "text")?;
        let len = usize::try_from(len).or_else(|_| self.err("length overflow"))?;
        let raw = self.take(len)?;
        std::str::from_utf8(raw).or_else(|_| self.err("invalid utf-8 in text"))
    }

    /// Begin an array; returns its length. Call [`Decoder::end`] when done.
    pub fn array(&mut self) -> Result<usize> {
        let len = self.expect_major(MT_ARRAY, "array")?;
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return self.err("nesting too deep");
        }
        // Each element takes at least one byte; reject absurd lengths early.
        if len > (self.data.len() - self.pos) as u64 {
            return self.err("array length exceeds input");
        }
        Ok(len as usize)
    }

    pub fn array_exact(&mut self, n: usize) -> Result<()> {
        let len = self.array()?;
        if len != n {
            return self.err(format!("expected array of {n}, got {len}"));
        }
        Ok(())
    }

    pub fn end(&mut self) {
        self.depth -= 1;
    }

    /// Returns true (and consumes) if the next item is `null`.
    pub fn null(&mut self) -> Result<bool> {
        if self.peek()? == NULL {
            self.pos += 1;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub fn bool(&mut self) -> Result<bool> {
        match self.take(1)?[0] {
            TRUE => Ok(true),
            FALSE => Ok(false),
            _ => self.err("expected bool"),
        }
    }

    /// Snapshot the read position (for one-item lookahead).
    pub fn clone_state(&self) -> (usize, usize) {
        (self.pos, self.depth)
    }

    pub fn restore_state(&mut self, state: (usize, usize)) {
        (self.pos, self.depth) = state;
    }

    pub fn finish(self) -> Result<()> {
        if self.pos != self.data.len() {
            return Err(Error::Decode(format!("{} trailing bytes", self.data.len() - self.pos)));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc_vectors() {
        // RFC 8949 Appendix A.
        let cases: &[(i64, &str)] = &[
            (0, "00"),
            (23, "17"),
            (24, "1818"),
            (100, "1864"),
            (1000, "1903e8"),
            (1_000_000, "1a000f4240"),
            (1_000_000_000_000, "1b000000e8d4a51000"),
            (-1, "20"),
            (-100, "3863"),
            (-1000, "3903e7"),
        ];
        for (v, hex_str) in cases {
            let mut e = Encoder::new();
            e.int(*v);
            assert_eq!(hex::encode(e.into_bytes()), *hex_str, "encoding {v}");
            let raw = hex::decode(hex_str).unwrap();
            let mut d = Decoder::new(&raw);
            assert_eq!(d.int().unwrap(), *v);
            d.finish().unwrap();
        }
    }

    #[test]
    fn rejects_non_shortest() {
        // 5 encoded with a 1-byte follow-on.
        let raw = [0x18, 0x05];
        assert_eq!(Decoder::new(&raw).uint(), Err(Error::NonCanonical));
        let raw = [0x19, 0x00, 0xff];
        assert_eq!(Decoder::new(&raw).uint(), Err(Error::NonCanonical));
    }

    #[test]
    fn rejects_indefinite() {
        let raw = [0x9f, 0xff];
        assert!(Decoder::new(&raw).array().is_err());
    }

    #[test]
    fn rejects_huge_array_len() {
        let raw = [0x9a, 0xff, 0xff, 0xff, 0xff];
        assert!(Decoder::new(&raw).array().is_err());
    }

    #[test]
    fn int_extremes() {
        for v in [i64::MIN, i64::MAX, -24, -25, 0] {
            let mut e = Encoder::new();
            e.int(v);
            let b = e.into_bytes();
            assert_eq!(Decoder::new(&b).int().unwrap(), v);
        }
    }
}
