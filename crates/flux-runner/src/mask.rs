//! Streaming secret masking: every registered value is replaced with `***`,
//! even when it is split across writes.

const MASK: &[u8] = b"***";

#[derive(Debug)]
pub struct Masker {
    /// Longest first, so the longest match wins.
    secrets: Vec<Vec<u8>>,
    first: [bool; 256],
    /// Tail of earlier input that may be the start of a secret.
    held: Vec<u8>,
}

impl Default for Masker {
    fn default() -> Self {
        Masker { secrets: Vec::new(), first: [false; 256], held: Vec::new() }
    }
}

impl Masker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a value to mask. Multi-line values are also masked line by
    /// line, since tools often print them that way.
    pub fn add(&mut self, value: &str) {
        let mut parts = vec![value];
        if value.contains('\n') {
            parts.extend(value.lines().map(|l| l.trim_end_matches('\r')));
        }
        for p in parts {
            let p = p.as_bytes();
            if p.is_empty() || self.secrets.iter().any(|s| s == p) {
                continue;
            }
            self.first[p[0] as usize] = true;
            self.secrets.push(p.to_vec());
        }
        self.secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
    }

    /// Mask `data`; bytes that could begin a secret are held back until the
    /// next call (or [`Masker::finish`]).
    pub fn push(&mut self, data: &[u8]) -> Vec<u8> {
        self.held.extend_from_slice(data);
        self.process(false)
    }

    /// Mask and return everything held back.
    pub fn finish(&mut self) -> Vec<u8> {
        self.process(true)
    }

    fn process(&mut self, last: bool) -> Vec<u8> {
        let buf = std::mem::take(&mut self.held);
        if self.secrets.is_empty() {
            return buf;
        }
        let mut out = Vec::with_capacity(buf.len());
        let mut i = 0;
        'scan: while i < buf.len() {
            if !self.first[buf[i] as usize] {
                out.push(buf[i]);
                i += 1;
                continue;
            }
            let rest = &buf[i..];
            // A longer secret might still complete with more input: wait.
            if !last && self.secrets.iter().any(|s| s.len() > rest.len() && s.starts_with(rest)) {
                self.held = rest.to_vec();
                return out;
            }
            for s in &self.secrets {
                if rest.starts_with(s) {
                    out.extend_from_slice(MASK);
                    i += s.len();
                    continue 'scan;
                }
            }
            out.push(buf[i]);
            i += 1;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(m: &mut Masker, chunks: &[&str]) -> String {
        let mut out = Vec::new();
        for c in chunks {
            out.extend(m.push(c.as_bytes()));
        }
        out.extend(m.finish());
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn masks_whole_values() {
        let mut m = Masker::new();
        assert_eq!(run(&mut m, &["nothing registered"]), "nothing registered");
        m.add("hunter2");
        m.add("");
        assert_eq!(run(&mut m, &["pw=hunter2; again hunter2!"]), "pw=***; again ***!");
        assert_eq!(run(&mut m, &["hunter"]), "hunter");
    }

    #[test]
    fn masks_across_chunk_boundaries() {
        let mut m = Masker::new();
        m.add("s3cr3t-value");
        let text = "token: s3cr3t-value end";
        // Every possible split point, and byte-at-a-time.
        for cut in 0..=text.len() {
            assert_eq!(run(&mut m, &[&text[..cut], &text[cut..]]), "token: *** end", "cut at {cut}");
        }
        let bytes: Vec<String> = text.chars().map(String::from).collect();
        let refs: Vec<&str> = bytes.iter().map(String::as_str).collect();
        assert_eq!(run(&mut m, &refs), "token: *** end");
        // Held bytes are released once they cannot be a secret.
        assert_eq!(String::from_utf8(m.push(b"s3cr")).unwrap(), "");
        assert_eq!(String::from_utf8(m.push(b"ap")).unwrap(), "s3crap");
        assert_eq!(String::from_utf8(m.finish()).unwrap(), "");
        // A prefix at the very end is flushed on finish.
        assert_eq!(run(&mut m, &["ends with s3cr3t-val"]), "ends with s3cr3t-val");
    }

    #[test]
    fn longest_match_and_multiline() {
        let mut m = Masker::new();
        m.add("abc");
        m.add("abcdef");
        assert_eq!(run(&mut m, &["xabcde", "fy abc abcd"]), "x***y *** ***d");
        m.add("line one\nline two");
        assert_eq!(run(&mut m, &["line one\nline two|line two"]), "***|***");
    }
}
