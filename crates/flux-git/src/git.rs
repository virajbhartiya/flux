//! Thin wrappers over git plumbing: long-running `cat-file --batch` and
//! streaming `diff-tree --stdin`. Using git itself (rather than a library)
//! guarantees we see exactly what git sees, including odd legacy objects.

use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use anyhow::{Context, Result, anyhow, bail};

pub type Sha = [u8; 20];

pub fn parse_sha(s: &str) -> Result<Sha> {
    let v = hex::decode(s.trim()).with_context(|| format!("bad sha {s:?}"))?;
    v.try_into().map_err(|_| anyhow!("expected 20-byte sha1 (sha256 repos are not supported yet): {s:?}"))
}

pub fn git(repo: &Path) -> Command {
    let mut c = Command::new("git");
    // Import exactly what's in the object store: no replace refs or grafts.
    c.env("GIT_NO_REPLACE_OBJECTS", "1").arg("-C").arg(repo);
    c
}

pub fn run(repo: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let out = git(repo).args(args).output().with_context(|| format!("running git {args:?}"))?;
    if !out.status.success() {
        bail!("git {:?} failed: {}", args, String::from_utf8_lossy(&out.stderr));
    }
    Ok(out.stdout)
}

/// `git cat-file --batch` session.
pub struct CatFile {
    child: Child,
    stdin: BufWriter<ChildStdin>,
    stdout: BufReader<ChildStdout>,
}

impl CatFile {
    pub fn new(repo: &Path) -> Result<Self> {
        let mut child = git(repo)
            .args(["cat-file", "--batch"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .context("spawning git cat-file")?;
        let stdin = BufWriter::new(child.stdin.take().unwrap());
        let stdout = BufReader::with_capacity(1 << 20, child.stdout.take().unwrap());
        Ok(Self { child, stdin, stdout })
    }

    /// Returns (type, data).
    pub fn read(&mut self, sha: &Sha) -> Result<(String, Vec<u8>)> {
        writeln!(self.stdin, "{}", hex::encode(sha))?;
        self.stdin.flush()?;
        let mut header = String::new();
        self.stdout.read_line(&mut header)?;
        let parts: Vec<&str> = header.trim_end().split(' ').collect();
        if parts.len() == 2 && parts[1] == "missing" {
            bail!("git object {} missing", hex::encode(sha));
        }
        if parts.len() != 3 {
            bail!("unexpected cat-file header {header:?}");
        }
        let size: usize = parts[2].parse()?;
        let mut data = vec![0u8; size];
        self.stdout.read_exact(&mut data)?;
        let mut nl = [0u8; 1];
        self.stdout.read_exact(&mut nl)?;
        Ok((parts[1].to_string(), data))
    }
}

impl Drop for CatFile {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// One raw diff-tree entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawEntry {
    pub old_mode: u32,
    pub new_mode: u32,
    pub old_sha: Sha,
    pub new_sha: Sha,
    pub status: u8,
    pub path: Vec<u8>,
}

/// A diff-tree request: `commit` against `parent` (or the empty tree).
#[derive(Debug, Clone)]
pub struct DiffRequest {
    pub commit: Sha,
    pub parent: Option<Sha>,
}

/// Streaming `git diff-tree --stdin`: all requests are written by a
/// background thread; results are read back in order.
pub struct DiffTree {
    child: Child,
    stdout: BufReader<ChildStdout>,
    writer: Option<std::thread::JoinHandle<Result<()>>>,
    pending_header: Option<Vec<u8>>,
}

impl DiffTree {
    pub fn spawn(repo: &Path, requests: Vec<DiffRequest>) -> Result<Self> {
        let mut child = git(repo)
            .args(["diff-tree", "--stdin", "-r", "-z", "--raw", "--no-renames", "--root", "--always", "--full-index"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .context("spawning git diff-tree")?;
        let stdin = child.stdin.take().unwrap();
        let writer = std::thread::spawn(move || -> Result<()> {
            let mut w = BufWriter::new(stdin);
            for r in requests {
                match r.parent {
                    Some(p) => writeln!(w, "{} {}", hex::encode(r.commit), hex::encode(p))?,
                    None => writeln!(w, "{}", hex::encode(r.commit))?,
                }
            }
            w.flush()?;
            Ok(())
        });
        let stdout = BufReader::with_capacity(1 << 20, child.stdout.take().unwrap());
        Ok(Self { child, stdout, writer: Some(writer), pending_header: None })
    }

    fn token(&mut self) -> Result<Option<Vec<u8>>> {
        let mut buf = Vec::new();
        let n = self.stdout.read_until(0, &mut buf)?;
        if n == 0 {
            return Ok(None);
        }
        if buf.last() == Some(&0) {
            buf.pop();
        }
        Ok(Some(buf))
    }

    /// Read the next record (entries for one request, in order).
    pub fn next_record(&mut self) -> Result<Vec<RawEntry>> {
        // Header.
        let header = match self.pending_header.take() {
            Some(h) => h,
            None => self.token()?.ok_or_else(|| anyhow!("diff-tree ended early"))?,
        };
        if header.first() == Some(&b':') {
            bail!("diff-tree: expected header, got entry");
        }
        let mut entries = Vec::new();
        loop {
            let Some(tok) = self.token()? else { break };
            if tok.first() != Some(&b':') {
                self.pending_header = Some(tok);
                break;
            }
            let meta = std::str::from_utf8(&tok[1..])?;
            let f: Vec<&str> = meta.split(' ').collect();
            if f.len() != 5 {
                bail!("bad diff-tree entry {meta:?}");
            }
            let path = self.token()?.ok_or_else(|| anyhow!("diff-tree: missing path"))?;
            entries.push(RawEntry {
                old_mode: u32::from_str_radix(f[0], 8)?,
                new_mode: u32::from_str_radix(f[1], 8)?,
                old_sha: parse_sha(f[2])?,
                new_sha: parse_sha(f[3])?,
                status: f[4].as_bytes()[0],
                path,
            });
        }
        Ok(entries)
    }

    pub fn finish(mut self) -> Result<()> {
        if let Some(w) = self.writer.take() {
            w.join().map_err(|_| anyhow!("diff-tree writer panicked"))??;
        }
        self.child.wait()?;
        Ok(())
    }
}

/// Parsed git commit object. Unknown headers are preserved verbatim.
#[derive(Debug, Clone)]
pub struct Commit {
    pub parents: Vec<Sha>,
    pub author: Vec<u8>,
    pub author_time: (i64, i16),
    pub committer: Vec<u8>,
    pub committer_time: (i64, i16),
    /// Raw bytes of all headers other than tree/parent/author/committer,
    /// including continuation lines, in original order.
    pub extra_headers: Vec<u8>,
    pub message: Vec<u8>,
    /// True if author/committer lines were not in canonical form.
    pub raw_identity: Option<(Vec<u8>, Vec<u8>)>,
}

/// Parse `Name <email> 1234567890 +0530` → (ident, (unix_s, tz_min)).
fn parse_ident(line: &[u8]) -> Option<(Vec<u8>, (i64, i16))> {
    let s = line;
    let tz_sp = s.iter().rposition(|b| *b == b' ')?;
    let tz = std::str::from_utf8(&s[tz_sp + 1..]).ok()?;
    let ts_sp = s[..tz_sp].iter().rposition(|b| *b == b' ')?;
    let ts: i64 = std::str::from_utf8(&s[ts_sp + 1..tz_sp]).ok()?.parse().ok()?;
    if tz.len() != 5 || !(tz.starts_with('+') || tz.starts_with('-')) {
        return None;
    }
    let hh: i16 = tz[1..3].parse().ok()?;
    let mm: i16 = tz[3..5].parse().ok()?;
    let mut off = hh * 60 + mm;
    if tz.starts_with('-') {
        off = -off;
    }
    // Round-trip check: "-0000" can't be represented, keep it raw.
    if tz == "-0000" {
        return None;
    }
    Some((s[..ts_sp].to_vec(), (ts, off)))
}

pub fn parse_commit(data: &[u8]) -> Result<Commit> {
    let split = data.windows(2).position(|w| w == b"\n\n");
    let (head, message) = match split {
        Some(i) => (&data[..i], data[i + 2..].to_vec()),
        None => (data, Vec::new()),
    };
    let mut parents = Vec::new();
    let mut author_line = None;
    let mut committer_line = None;
    let mut extra = Vec::new();
    let mut in_extra = false;
    for line in head.split(|b| *b == b'\n') {
        if line.starts_with(b" ") {
            if in_extra {
                extra.extend_from_slice(line);
                extra.push(b'\n');
            }
            continue;
        }
        in_extra = false;
        if let Some(rest) = line.strip_prefix(b"tree ") {
            let _ = rest;
        } else if let Some(rest) = line.strip_prefix(b"parent ") {
            parents.push(parse_sha(std::str::from_utf8(rest)?)?);
        } else if let Some(rest) = line.strip_prefix(b"author ") {
            author_line = Some(rest.to_vec());
        } else if let Some(rest) = line.strip_prefix(b"committer ") {
            committer_line = Some(rest.to_vec());
        } else {
            in_extra = true;
            extra.extend_from_slice(line);
            extra.push(b'\n');
        }
    }
    let author_line = author_line.unwrap_or_default();
    let committer_line = committer_line.unwrap_or_default();
    let (author, author_time, committer, committer_time, raw_identity) =
        match (parse_ident(&author_line), parse_ident(&committer_line)) {
            (Some((a, at)), Some((c, ct))) => (a, at, c, ct, None),
            _ => (Vec::new(), (0, 0), Vec::new(), (0, 0), Some((author_line, committer_line))),
        };
    Ok(Commit { parents, author, author_time, committer, committer_time, extra_headers: extra, message, raw_identity })
}

/// List `(ref name, sha)` for `refs/heads/*` (or the given patterns).
pub fn list_refs(repo: &Path, patterns: &[String]) -> Result<Vec<(String, Sha)>> {
    let mut args = vec!["for-each-ref", "--format=%(objectname) %(objecttype) %(refname)"];
    let pats: Vec<&str> = if patterns.is_empty() { vec!["refs/heads/"] } else { patterns.iter().map(String::as_str).collect() };
    args.extend(pats);
    let out = String::from_utf8(run(repo, &args)?)?;
    let mut refs = Vec::new();
    for line in out.lines() {
        let mut it = line.splitn(3, ' ');
        let (sha, ty, name) = (it.next().unwrap_or(""), it.next().unwrap_or(""), it.next().unwrap_or(""));
        if ty == "commit" {
            refs.push((name.to_string(), parse_sha(sha)?));
        }
    }
    Ok(refs)
}

/// Commits reachable from `tips`, parents before children, with parents.
pub fn rev_list(repo: &Path, tips: &[Sha]) -> Result<Vec<(Sha, Vec<Sha>)>> {
    if tips.is_empty() {
        return Ok(vec![]);
    }
    let mut child = git(repo)
        .args(["rev-list", "--topo-order", "--reverse", "--parents", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    {
        let mut w = child.stdin.take().unwrap();
        for t in tips {
            writeln!(w, "{}", hex::encode(t))?;
        }
    }
    let mut out = String::new();
    child.stdout.take().unwrap().read_to_string(&mut out)?;
    if !child.wait()?.success() {
        bail!("git rev-list failed");
    }
    out.lines()
        .map(|l| {
            let mut it = l.split(' ');
            let c = parse_sha(it.next().unwrap_or(""))?;
            let ps = it.map(parse_sha).collect::<Result<Vec<_>>>()?;
            Ok((c, ps))
        })
        .collect()
}

/// `git ls-tree -r -z --full-tree <commit>` → (mode, sha, path).
pub fn ls_tree(repo: &Path, commit: &Sha) -> Result<Vec<(u32, Sha, Vec<u8>)>> {
    let out = run(repo, &["ls-tree", "-r", "-z", "--full-tree", &hex::encode(commit)])?;
    let mut entries = Vec::new();
    for rec in out.split(|b| *b == 0).filter(|r| !r.is_empty()) {
        let tab = rec.iter().position(|b| *b == b'\t').ok_or_else(|| anyhow!("bad ls-tree line"))?;
        let meta = std::str::from_utf8(&rec[..tab])?;
        let f: Vec<&str> = meta.split(' ').collect();
        entries.push((u32::from_str_radix(f[0], 8)?, parse_sha(f[2])?, rec[tab + 1..].to_vec()));
    }
    Ok(entries)
}

pub fn repo_dir(p: &Path) -> Result<PathBuf> {
    let out = run(p, &["rev-parse", "--git-dir"])?;
    let gd = PathBuf::from(String::from_utf8(out)?.trim());
    Ok(if gd.is_absolute() { gd } else { p.join(gd) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_commit() {
        let raw = b"tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n\
parent 9e6574d88b8c7bef929ed2d6bc464e88459df394\n\
author Ada <ada@x.org> 1700000000 +0530\n\
committer Bob <bob@x.org> 1700000100 -0800\n\
gpgsig -----BEGIN PGP SIGNATURE-----\n \n abc\n -----END PGP SIGNATURE-----\n\
\n\
subject\n\nbody\n";
        let c = parse_commit(raw).unwrap();
        assert_eq!(c.parents.len(), 1);
        assert_eq!(c.author, b"Ada <ada@x.org>");
        assert_eq!(c.author_time, (1_700_000_000, 330));
        assert_eq!(c.committer_time, (1_700_000_100, -480));
        assert!(c.extra_headers.starts_with(b"gpgsig -----BEGIN"));
        assert!(c.extra_headers.ends_with(b" -----END PGP SIGNATURE-----\n"));
        assert_eq!(c.message, b"subject\n\nbody\n");
        assert!(c.raw_identity.is_none());
    }
}
