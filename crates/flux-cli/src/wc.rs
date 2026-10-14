//! Working-copy state (SPEC §13.1–13.2).
//!
//! The dirstate is a compact binary file (`.flux/dirstate`) holding, per
//! tracked file, its kind, content id, size and mtime. `status` stats every
//! tracked file in parallel and only reads files whose stat data changed;
//! untracked files are found with a parallel walk that honours `.gitignore`
//! and `.fluxignore`.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use flux_core::{ChangesetId, ContentId, FileKind, RepoPath, TreeId};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

pub const META_DIR: &str = ".flux";
const STATE_MAGIC: &[u8; 4] = b"FXDS";
const STATE_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub server: String,
    pub repo: String,
    /// Directory prefixes to materialize; empty = everything.
    #[serde(default)]
    pub sparse: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    Regular,
    Executable,
    Symlink,
    GitLink,
}

impl Kind {
    pub fn from_core(k: &FileKind) -> Self {
        match k.normalized() {
            FileKind::Regular | FileKind::GitMode(_) => Kind::Regular,
            FileKind::Executable => Kind::Executable,
            FileKind::Symlink => Kind::Symlink,
            FileKind::GitLink(_) => Kind::GitLink,
        }
    }

    fn byte(self) -> u8 {
        match self {
            Kind::Regular => 0,
            Kind::Executable => 1,
            Kind::Symlink => 2,
            Kind::GitLink => 3,
        }
    }

    fn from_byte(b: u8) -> Result<Self> {
        Ok(match b {
            0 => Kind::Regular,
            1 => Kind::Executable,
            2 => Kind::Symlink,
            3 => Kind::GitLink,
            _ => bail!("corrupt dirstate: bad kind {b}"),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    pub kind: Kind,
    /// Content id bytes; for gitlinks the submodule sha in the first 20.
    pub id: [u8; 32],
    pub size: u64,
    pub mtime_ns: i128,
}

impl Entry {
    pub fn content_id(&self) -> Option<ContentId> {
        (self.kind != Kind::GitLink).then(|| ContentId::from_bytes(self.id))
    }
}

#[derive(Debug, Clone, Default)]
pub struct State {
    pub parent: Option<ChangesetId>,
    pub root_tree: Option<TreeId>,
    /// Ref the working copy follows; `commit` advances it.
    pub bookmark: Option<String>,
    pub files: BTreeMap<RepoPath, Entry>,
}

impl State {
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(64 + self.files.len() * 96);
        out.extend_from_slice(STATE_MAGIC);
        out.extend_from_slice(&STATE_VERSION.to_le_bytes());
        let opt = |out: &mut Vec<u8>, v: Option<&[u8; 32]>| match v {
            Some(b) => {
                out.push(1);
                out.extend_from_slice(b);
            }
            None => out.push(0),
        };
        opt(&mut out, self.parent.as_ref().map(|p| p.as_bytes()));
        opt(&mut out, self.root_tree.as_ref().map(|t| t.as_bytes()));
        match &self.bookmark {
            Some(b) => {
                out.extend_from_slice(&(b.len() as u32).to_le_bytes());
                out.extend_from_slice(b.as_bytes());
            }
            None => out.extend_from_slice(&u32::MAX.to_le_bytes()),
        }
        out.extend_from_slice(&(self.files.len() as u64).to_le_bytes());
        for (p, e) in &self.files {
            out.extend_from_slice(&(p.as_bytes().len() as u32).to_le_bytes());
            out.extend_from_slice(p.as_bytes());
            out.push(e.kind.byte());
            out.extend_from_slice(&e.id);
            out.extend_from_slice(&e.size.to_le_bytes());
            out.extend_from_slice(&e.mtime_ns.to_le_bytes());
        }
        let sum = blake3::hash(&out);
        out.extend_from_slice(sum.as_bytes());
        out
    }

    fn decode(data: &[u8]) -> Result<Self> {
        if data.len() < 40 || &data[..4] != STATE_MAGIC {
            bail!("corrupt dirstate (bad header)");
        }
        let (body, sum) = data.split_at(data.len() - 32);
        if blake3::hash(body).as_bytes() != sum {
            bail!("corrupt dirstate (checksum mismatch); run `flux checkout --force @` to rebuild it");
        }
        let mut r = Reader { b: body, i: 4 };
        if r.u32()? != STATE_VERSION {
            bail!("unsupported dirstate version");
        }
        let parent = r.opt32()?.map(ChangesetId::from_bytes);
        let root_tree = r.opt32()?.map(TreeId::from_bytes);
        let bl = r.u32()?;
        let bookmark = if bl == u32::MAX { None } else { Some(String::from_utf8(r.take(bl as usize)?.to_vec())?) };
        let n = r.u64()?;
        let mut files = BTreeMap::new();
        for _ in 0..n {
            let pl = r.u32()? as usize;
            let path = RepoPath::new(r.take(pl)?.to_vec())?;
            let kind = Kind::from_byte(r.take(1)?[0])?;
            let id: [u8; 32] = r.take(32)?.try_into().unwrap();
            let size = r.u64()?;
            let mtime_ns = i128::from_le_bytes(r.take(16)?.try_into().unwrap());
            files.insert(path, Entry { kind, id, size, mtime_ns });
        }
        Ok(State { parent, root_tree, bookmark, files })
    }
}

struct Reader<'a> {
    b: &'a [u8],
    i: usize,
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8]> {
        let s = self.b.get(self.i..self.i + n).context("corrupt dirstate (truncated)")?;
        self.i += n;
        Ok(s)
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn opt32(&mut self) -> Result<Option<[u8; 32]>> {
        Ok(match self.take(1)?[0] {
            0 => None,
            _ => Some(self.take(32)?.try_into().unwrap()),
        })
    }
}

/// The phase-1 JSON dirstate, read once for migration.
mod legacy {
    use super::*;

    #[derive(Deserialize)]
    struct LegacyEntry {
        kind: Kind,
        id: String,
        size: u64,
        mtime_ns: i128,
    }

    #[derive(Deserialize)]
    struct LegacyState {
        parent: Option<ChangesetId>,
        root_tree: Option<TreeId>,
        bookmark: Option<String>,
        files: BTreeMap<String, LegacyEntry>,
    }

    pub fn load(data: &[u8]) -> Result<State> {
        let s: LegacyState = serde_json::from_slice(data)?;
        let mut files = BTreeMap::new();
        for (k, e) in s.files {
            let path = match k.strip_prefix("hex:") {
                Some(h) => RepoPath::new(hex::decode(h)?)?,
                None => RepoPath::new(k.as_bytes())?,
            };
            let mut id = [0u8; 32];
            if e.kind == Kind::GitLink {
                id[..20].copy_from_slice(&hex::decode(&e.id)?);
            } else {
                id = *ContentId::parse(&e.id)?.as_bytes();
            }
            files.insert(path, Entry { kind: e.kind, id, size: e.size, mtime_ns: e.mtime_ns });
        }
        Ok(State { parent: s.parent, root_tree: s.root_tree, bookmark: s.bookmark, files })
    }
}

pub struct WorkingCopy {
    pub root: PathBuf,
    pub config: Config,
    pub state: State,
}

#[cfg(unix)]
pub fn mtime_ns(md: &fs::Metadata) -> i128 {
    md.mtime() as i128 * 1_000_000_000 + md.mtime_nsec() as i128
}

#[cfg(not(unix))]
pub fn mtime_ns(md: &fs::Metadata) -> i128 {
    md.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_nanos() as i128).unwrap_or(0)
}

#[cfg(unix)]
fn is_exec(md: &fs::Metadata) -> bool {
    md.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_exec(_: &fs::Metadata) -> bool {
    false
}

fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    let mut f = fs::File::create(&tmp)?;
    f.write_all(data)?;
    f.sync_all()?;
    fs::rename(&tmp, path)?;
    Ok(())
}

impl WorkingCopy {
    pub fn find(start: &Path) -> Result<Self> {
        let mut dir = start.canonicalize()?;
        loop {
            if dir.join(META_DIR).join("config.json").exists() {
                return Self::open(&dir);
            }
            if !dir.pop() {
                bail!("not inside a flux working copy (no {META_DIR}/ found); use `flux attach`");
            }
        }
    }

    pub fn open(root: &Path) -> Result<Self> {
        let meta = root.join(META_DIR);
        let config: Config = serde_json::from_slice(&fs::read(meta.join("config.json"))?)?;
        let state = match fs::read(meta.join("dirstate")) {
            Ok(b) => State::decode(&b)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => match fs::read(meta.join("state.json")) {
                Ok(b) => legacy::load(&b).context("migrating the phase-1 dirstate")?,
                Err(_) => State::default(),
            },
            Err(e) => return Err(e.into()),
        };
        Ok(Self { root: root.to_path_buf(), config, state })
    }

    pub fn init(root: &Path, config: Config) -> Result<Self> {
        let meta = root.join(META_DIR);
        if meta.exists() {
            bail!("{} is already a flux working copy", root.display());
        }
        fs::create_dir_all(&meta)?;
        let wc = Self { root: root.canonicalize()?, config, state: State::default() };
        wc.save_config()?;
        wc.save_state()?;
        Ok(wc)
    }

    pub fn save_config(&self) -> Result<()> {
        write_atomic(&self.root.join(META_DIR).join("config.json"), &serde_json::to_vec_pretty(&self.config)?)
    }

    pub fn save_state(&self) -> Result<()> {
        write_atomic(&self.root.join(META_DIR).join("dirstate"), &self.state.encode())?;
        let _ = fs::remove_file(self.root.join(META_DIR).join("state.json"));
        Ok(())
    }

    pub fn sparse_prefixes(&self) -> Vec<Vec<u8>> {
        self.config.sparse.iter().map(|s| s.trim_matches('/').as_bytes().to_vec()).collect()
    }

    /// Is `path` inside the sparse profile?
    pub fn in_sparse(&self, path: &[u8]) -> bool {
        self.config.sparse.is_empty()
            || self.config.sparse.iter().any(|p| {
                let p = p.trim_matches('/').as_bytes();
                path.starts_with(p) && (path.len() == p.len() || path[p.len()] == b'/')
            })
    }

    /// Should the walker descend into directory `path`?
    fn descend(&self, path: &[u8]) -> bool {
        self.in_sparse(path) || self.sparse_prefixes().iter().any(|p| p.starts_with(path) && p.get(path.len()) == Some(&b'/'))
    }

    #[cfg(unix)]
    pub fn fs_path(&self, p: &RepoPath) -> PathBuf {
        self.root.join(std::ffi::OsStr::from_bytes(p.as_bytes()))
    }

    #[cfg(not(unix))]
    pub fn fs_path(&self, p: &RepoPath) -> PathBuf {
        self.root.join(p.to_string())
    }

    /// Read a file's bytes as flux sees them (symlink → target bytes).
    pub fn read_entry(&self, p: &RepoPath) -> Result<Option<(Kind, Vec<u8>, fs::Metadata)>> {
        let fp = self.fs_path(p);
        let md = match fs::symlink_metadata(&fp) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        if md.file_type().is_symlink() {
            let target = fs::read_link(&fp)?;
            #[cfg(unix)]
            let bytes = target.as_os_str().as_bytes().to_vec();
            #[cfg(not(unix))]
            let bytes = target.to_string_lossy().into_owned().into_bytes();
            return Ok(Some((Kind::Symlink, bytes, md)));
        }
        if md.is_dir() {
            return Ok(None);
        }
        let kind = if is_exec(&md) { Kind::Executable } else { Kind::Regular };
        Ok(Some((kind, fs::read(&fp)?, md)))
    }

    /// Write a file atomically (temp + rename).
    pub fn write_file(&self, p: &RepoPath, kind: Kind, data: &[u8]) -> Result<fs::Metadata> {
        let fp = self.fs_path(p);
        let parent = fp.parent().unwrap();
        // A file may sit where a directory is needed (file→dir transition).
        let mut anc = Some(parent);
        while let Some(a) = anc {
            if a == self.root {
                break;
            }
            if fs::symlink_metadata(a).is_ok_and(|m| !m.is_dir()) {
                fs::remove_file(a)?;
                break;
            }
            anc = a.parent();
        }
        fs::create_dir_all(parent)?;
        if fs::symlink_metadata(&fp).is_ok_and(|m| m.is_dir()) && kind != Kind::GitLink {
            fs::remove_dir_all(&fp)?;
        }
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = parent.join(format!(".flux-tmp-{}-{n}", std::process::id()));
        let _ = fs::remove_file(&tmp);
        match kind {
            Kind::Symlink => {
                #[cfg(unix)]
                std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(data), &tmp)?;
                #[cfg(not(unix))]
                fs::write(&tmp, data)?;
            }
            Kind::GitLink => {
                fs::create_dir_all(&fp)?;
                return Ok(fs::symlink_metadata(&fp)?);
            }
            Kind::Regular | Kind::Executable => {
                let mut f = fs::File::create(&tmp)?;
                f.write_all(data)?;
                #[cfg(unix)]
                f.set_permissions(fs::Permissions::from_mode(if kind == Kind::Executable { 0o755 } else { 0o644 }))?;
            }
        }
        fs::rename(&tmp, &fp).with_context(|| format!("writing {}", fp.display()))?;
        Ok(fs::symlink_metadata(&fp)?)
    }

    /// Write many files in parallel (checkout's hot path). Parent
    /// directories are prepared once; new files are written in place,
    /// existing ones are replaced atomically.
    pub fn write_files(&self, files: Vec<(RepoPath, Kind, Vec<u8>)>) -> Result<Vec<(RepoPath, fs::Metadata)>> {
        let mut dirs: Vec<PathBuf> =
            files.iter().filter_map(|(p, _, _)| self.fs_path(p).parent().map(Path::to_path_buf)).collect();
        dirs.sort();
        dirs.dedup();
        for d in &dirs {
            if d.is_dir() {
                continue;
            }
            // A file may sit where a directory is needed (file→dir).
            let mut anc = Some(d.as_path());
            while let Some(a) = anc {
                if a == self.root {
                    break;
                }
                if fs::symlink_metadata(a).is_ok_and(|m| !m.is_dir()) {
                    fs::remove_file(a)?;
                    break;
                }
                anc = a.parent();
            }
            fs::create_dir_all(d)?;
        }
        files
            .into_par_iter()
            .map(|(p, kind, data)| {
                if matches!(kind, Kind::Symlink | Kind::GitLink) {
                    let md = self.write_file(&p, kind, &data)?;
                    return Ok((p, md));
                }
                let fp = self.fs_path(&p);
                match fs::symlink_metadata(&fp) {
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        let mut f = fs::File::create(&fp).with_context(|| format!("writing {}", fp.display()))?;
                        f.write_all(&data)?;
                        #[cfg(unix)]
                        if kind == Kind::Executable {
                            f.set_permissions(fs::Permissions::from_mode(0o755))?;
                        }
                        drop(f);
                        Ok((p, fs::symlink_metadata(&fp)?))
                    }
                    _ => {
                        let md = self.write_file(&p, kind, &data)?;
                        Ok((p, md))
                    }
                }
            })
            .collect()
    }

    pub fn remove_file(&self, p: &RepoPath) -> Result<()> {
        let fp = self.fs_path(p);
        match fs::symlink_metadata(&fp) {
            Ok(m) if m.is_dir() => fs::remove_dir_all(&fp)?,
            Ok(_) => fs::remove_file(&fp)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        // Prune now-empty parent directories.
        let mut dir = fp.parent().map(Path::to_path_buf);
        while let Some(d) = dir {
            if d == self.root || fs::remove_dir(&d).is_err() {
                break;
            }
            dir = d.parent().map(Path::to_path_buf);
        }
        Ok(())
    }

    /// Untracked files on disk (within the sparse profile), honouring
    /// `.gitignore` and `.fluxignore`.
    fn untracked(&self) -> Result<Vec<RepoPath>> {
        let found = std::sync::Mutex::new(Vec::new());
        let root = self.root.clone();
        let walker = ignore::WalkBuilder::new(&self.root)
            .hidden(false)
            .git_ignore(true)
            .git_global(false)
            .git_exclude(false)
            .require_git(false)
            .parents(false)
            .add_custom_ignore_filename(".fluxignore")
            .filter_entry(move |e| {
                let name = e.file_name();
                !(name == META_DIR || name == ".git")
            })
            .build_parallel();
        walker.run(|| {
            let found = &found;
            let root = &root;
            Box::new(move |res| {
                let Ok(entry) = res else { return ignore::WalkState::Continue };
                let Ok(rel) = entry.path().strip_prefix(root) else { return ignore::WalkState::Continue };
                if rel.as_os_str().is_empty() {
                    return ignore::WalkState::Continue;
                }
                #[cfg(unix)]
                let rel_bytes = rel.as_os_str().as_bytes().to_vec();
                #[cfg(not(unix))]
                let rel_bytes = rel.to_string_lossy().replace('\\', "/").into_bytes();
                let is_dir = entry.file_type().is_some_and(|t| t.is_dir());
                if is_dir {
                    if !self.descend(&rel_bytes) {
                        return ignore::WalkState::Skip;
                    }
                    // Submodule placeholders are tracked as single entries.
                    if let Ok(p) = RepoPath::new(rel_bytes.clone())
                        && self.state.files.get(&p).is_some_and(|e| e.kind == Kind::GitLink)
                    {
                        return ignore::WalkState::Skip;
                    }
                    return ignore::WalkState::Continue;
                }
                if !self.in_sparse(&rel_bytes) {
                    return ignore::WalkState::Continue;
                }
                if let Ok(p) = RepoPath::new(rel_bytes)
                    && !self.state.files.contains_key(&p)
                {
                    found.lock().unwrap().push(p);
                }
                ignore::WalkState::Continue
            })
        });
        let mut v = found.into_inner().unwrap();
        v.sort();
        Ok(v)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Modified,
    Deleted,
    Untracked,
}

/// A detected working-copy change. `new` holds the file's current bytes when
/// they were read during status (modified files); untracked files are read
/// on demand by [`WorkingCopy::load_change`].
#[derive(Debug, Clone)]
pub struct StatusEntry {
    pub path: RepoPath,
    pub change: Change,
    pub new: Option<(Kind, Vec<u8>)>,
}

impl WorkingCopy {
    /// Compare disk against the dirstate. Tracked files are stat'ed in
    /// parallel; only files whose stat data changed are read and hashed.
    pub fn status(&self) -> Result<Vec<StatusEntry>> {
        let tracked: Vec<(&RepoPath, &Entry)> = self.state.files.iter().collect();
        let results: Vec<Result<Option<StatusEntry>>> = tracked
            .par_iter()
            .map(|(p, e)| {
                if e.kind == Kind::GitLink {
                    return Ok(None);
                }
                let fp = self.fs_path(p);
                let md = match fs::symlink_metadata(&fp) {
                    Ok(m) => m,
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                        return Ok(Some(StatusEntry { path: (*p).clone(), change: Change::Deleted, new: None }));
                    }
                    Err(err) => return Err(err.into()),
                };
                if md.is_dir() {
                    return Ok(Some(StatusEntry { path: (*p).clone(), change: Change::Deleted, new: None }));
                }
                let kind = if md.file_type().is_symlink() {
                    Kind::Symlink
                } else if is_exec(&md) {
                    Kind::Executable
                } else {
                    Kind::Regular
                };
                if kind == e.kind && md.len() == e.size && mtime_ns(&md) == e.mtime_ns && e.size != 0 {
                    return Ok(None); // stat-clean
                }
                let Some((kind, data, _)) = self.read_entry(p)? else {
                    return Ok(Some(StatusEntry { path: (*p).clone(), change: Change::Deleted, new: None }));
                };
                if kind == e.kind && ContentId::hash(&data).as_bytes() == &e.id {
                    return Ok(None);
                }
                Ok(Some(StatusEntry { path: (*p).clone(), change: Change::Modified, new: Some((kind, data)) }))
            })
            .collect();
        let mut out = Vec::new();
        for r in results {
            out.extend(r?);
        }
        for p in self.untracked()? {
            out.push(StatusEntry { path: p, change: Change::Untracked, new: None });
        }
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }

    /// Ensure `new` is loaded for a change that will be committed.
    pub fn load_change(&self, e: &mut StatusEntry) -> Result<()> {
        if e.change == Change::Deleted || e.new.is_some() {
            return Ok(());
        }
        match self.read_entry(&e.path)? {
            Some((k, d, _)) => e.new = Some((k, d)),
            None => e.change = Change::Deleted,
        }
        Ok(())
    }

    /// Paths of the status filtered to `filters` (prefix match), if any.
    pub fn matches(path: &RepoPath, filters: &[RepoPath]) -> bool {
        filters.is_empty() || filters.iter().any(|f| path.starts_with(f))
    }
}

/// Resolve a user-supplied path (relative to the cwd) into a repo path.
pub fn repo_path_arg(wc: &WorkingCopy, arg: &str) -> Result<Option<RepoPath>> {
    let cwd = std::env::current_dir()?;
    let abs = if Path::new(arg).is_absolute() { PathBuf::from(arg) } else { cwd.join(arg) };
    let mut parts: Vec<String> = Vec::new();
    for c in abs.components() {
        match c {
            std::path::Component::ParentDir => {
                parts.pop();
            }
            std::path::Component::CurDir => {}
            std::path::Component::Normal(s) => parts.push(s.to_string_lossy().into_owned()),
            _ => parts.clear(),
        }
    }
    let norm = PathBuf::from("/").join(parts.join("/"));
    let root = wc.root.canonicalize().unwrap_or(wc.root.clone());
    let rel = norm
        .strip_prefix(&root)
        .or_else(|_| norm.strip_prefix(&wc.root))
        .with_context(|| format!("{arg} is outside the working copy"))?;
    let s = rel.to_string_lossy().replace('\\', "/");
    let s = s.trim_matches('/');
    if s.is_empty() {
        return Ok(None);
    }
    Ok(Some(RepoPath::new(s.as_bytes())?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dirstate_roundtrip_and_corruption() {
        let mut s = State { parent: Some(ChangesetId::hash(b"p")), bookmark: Some("main".into()), ..Default::default() };
        s.files.insert(
            RepoPath::try_from("a/b.txt").unwrap(),
            Entry { kind: Kind::Executable, id: [7; 32], size: 12, mtime_ns: 1_700_000_000_123_456_789 },
        );
        let enc = s.encode();
        let back = State::decode(&enc).unwrap();
        assert_eq!(back.parent, s.parent);
        assert_eq!(back.bookmark, s.bookmark);
        assert_eq!(back.files, s.files);
        let mut bad = enc.clone();
        bad[20] ^= 1;
        assert!(State::decode(&bad).is_err());
    }
}
