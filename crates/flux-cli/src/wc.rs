//! Working-copy state (SPEC §13.1–13.2).
//!
//! Phase 1 keeps the dirstate as JSON in `.flux/state.json`; the SQLite
//! dirstate and filesystem-watcher journal land with fluxd in phase 2.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use flux_core::{ChangesetId, ContentId, FileKind, RepoPath, TreeId};
use serde::{Deserialize, Serialize};

pub const META_DIR: &str = ".flux";

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
        match k {
            FileKind::Regular => Kind::Regular,
            FileKind::Executable => Kind::Executable,
            FileKind::Symlink => Kind::Symlink,
            FileKind::GitLink(_) => Kind::GitLink,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub kind: Kind,
    /// Content id, or for gitlinks the hex submodule sha.
    pub id: String,
    pub size: u64,
    pub mtime_ns: i128,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct State {
    pub parent: Option<ChangesetId>,
    pub root_tree: Option<TreeId>,
    /// Ref the working copy follows; `commit` advances it.
    pub bookmark: Option<String>,
    /// Keyed by lossy display path; `raw` paths are stored via escaping below.
    pub files: BTreeMap<String, Entry>,
}

pub struct WorkingCopy {
    pub root: PathBuf,
    pub config: Config,
    pub state: State,
}

/// Paths are bytes; we key JSON by hex for non-UTF-8 paths.
pub fn path_key(p: &RepoPath) -> String {
    match std::str::from_utf8(p.as_bytes()) {
        Ok(s) if !s.starts_with("hex:") => s.to_string(),
        _ => format!("hex:{}", hex_encode(p.as_bytes())),
    }
}

pub fn key_path(k: &str) -> Result<RepoPath> {
    Ok(match k.strip_prefix("hex:") {
        Some(h) => RepoPath::new(hex_decode(h)?)?,
        None => RepoPath::new(k.as_bytes())?,
    })
}

fn hex_encode(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn hex_decode(s: &str) -> Result<Vec<u8>> {
    (0..s.len()).step_by(2).map(|i| Ok(u8::from_str_radix(&s[i..i + 2], 16)?)).collect()
}

pub fn mtime_ns(md: &fs::Metadata) -> i128 {
    md.mtime() as i128 * 1_000_000_000 + md.mtime_nsec() as i128
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
        let state: State = match fs::read(meta.join("state.json")) {
            Ok(b) => serde_json::from_slice(&b)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => State::default(),
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

    fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
        let tmp = path.with_extension("tmp");
        let mut f = fs::File::create(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
        fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn save_config(&self) -> Result<()> {
        Self::write_atomic(&self.root.join(META_DIR).join("config.json"), &serde_json::to_vec_pretty(&self.config)?)
    }

    pub fn save_state(&self) -> Result<()> {
        Self::write_atomic(&self.root.join(META_DIR).join("state.json"), &serde_json::to_vec(&self.state)?)
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

    pub fn fs_path(&self, p: &RepoPath) -> PathBuf {
        self.root.join(std::ffi::OsStr::from_bytes(p.as_bytes()))
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
            return Ok(Some((Kind::Symlink, target.as_os_str().as_bytes().to_vec(), md)));
        }
        if md.is_dir() {
            return Ok(None);
        }
        let kind = if md.permissions().mode() & 0o111 != 0 { Kind::Executable } else { Kind::Regular };
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
        if fs::symlink_metadata(&fp).is_ok_and(|m| m.is_dir()) {
            fs::remove_dir_all(&fp)?;
        }
        let tmp = parent.join(format!(".flux-tmp-{}", std::process::id()));
        let _ = fs::remove_file(&tmp);
        match kind {
            Kind::Symlink => {
                std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(data), &tmp)?;
            }
            Kind::GitLink => {
                fs::create_dir_all(&fp)?;
                return Ok(fs::symlink_metadata(&fp)?);
            }
            Kind::Regular | Kind::Executable => {
                let mut f = fs::File::create(&tmp)?;
                f.write_all(data)?;
                let mode = if kind == Kind::Executable { 0o755 } else { 0o644 };
                f.set_permissions(fs::Permissions::from_mode(mode))?;
            }
        }
        fs::rename(&tmp, &fp).with_context(|| format!("writing {}", fp.display()))?;
        Ok(fs::symlink_metadata(&fp)?)
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

    /// All files on disk within the sparse profile (excluding `.flux`).
    pub fn scan(&self) -> Result<Vec<RepoPath>> {
        let mut out = Vec::new();
        let mut stack = vec![self.root.clone()];
        let tracked_gitlinks: std::collections::HashSet<&str> =
            self.state.files.iter().filter(|(_, e)| e.kind == Kind::GitLink).map(|(k, _)| k.as_str()).collect();
        while let Some(dir) = stack.pop() {
            for entry in fs::read_dir(&dir)? {
                let entry = entry?;
                let fp = entry.path();
                let rel = fp.strip_prefix(&self.root).unwrap();
                let rel_bytes = rel.as_os_str().as_bytes();
                if rel_bytes == META_DIR.as_bytes() || rel_bytes == b".git" {
                    continue;
                }
                let Ok(rp) = RepoPath::new(rel_bytes) else { continue };
                let ft = entry.file_type()?;
                if ft.is_dir() {
                    if tracked_gitlinks.contains(path_key(&rp).as_str()) {
                        out.push(rp);
                        continue;
                    }
                    let descend = self.config.sparse.is_empty()
                        || self.in_sparse(rel_bytes)
                        || self.sparse_prefixes().iter().any(|p| p.starts_with(rel_bytes) && p.get(rel_bytes.len()) == Some(&b'/'));
                    if descend {
                        stack.push(fp);
                    }
                } else if self.in_sparse(rel_bytes) {
                    out.push(rp);
                }
            }
        }
        out.sort();
        Ok(out)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Modified,
    Deleted,
    Untracked,
}

/// A detected working-copy change with the data needed to commit it.
#[derive(Debug, Clone)]
pub struct StatusEntry {
    pub path: RepoPath,
    pub change: Change,
    pub new: Option<(Kind, Vec<u8>)>,
}

impl WorkingCopy {
    /// Compare disk against the dirstate. Files whose size and mtime match the
    /// dirstate are assumed clean without reading them.
    pub fn status(&self) -> Result<Vec<StatusEntry>> {
        let mut out = Vec::new();
        let on_disk = self.scan()?;
        let disk_set: std::collections::HashSet<&RepoPath> = on_disk.iter().collect();
        for (key, entry) in &self.state.files {
            let p = key_path(key)?;
            if !disk_set.contains(&p) && entry.kind != Kind::GitLink {
                out.push(StatusEntry { path: p, change: Change::Deleted, new: None });
            }
        }
        for p in &on_disk {
            let key = path_key(p);
            let tracked = self.state.files.get(&key);
            if tracked.is_some_and(|e| e.kind == Kind::GitLink) {
                continue;
            }
            let Some((kind, data, md)) = self.read_entry_lazy(p, tracked)? else { continue };
            match tracked {
                None => out.push(StatusEntry { path: p.clone(), change: Change::Untracked, new: Some((kind, data)) }),
                Some(e) => {
                    if e.kind == kind && e.size == md.len() && e.mtime_ns == mtime_ns(&md) && data.is_empty() && e.size != 0 {
                        continue; // stat-clean
                    }
                    let id = ContentId::hash(&data).to_string();
                    if e.kind != kind || e.id != id {
                        out.push(StatusEntry { path: p.clone(), change: Change::Modified, new: Some((kind, data)) });
                    }
                }
            }
        }
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }

    /// Like `read_entry` but skips reading content when stat data matches.
    fn read_entry_lazy(&self, p: &RepoPath, tracked: Option<&Entry>) -> Result<Option<(Kind, Vec<u8>, fs::Metadata)>> {
        let fp = self.fs_path(p);
        let md = fs::symlink_metadata(&fp)?;
        if let Some(e) = tracked
            && !md.file_type().is_symlink()
            && md.is_file()
        {
            let kind = if md.permissions().mode() & 0o111 != 0 { Kind::Executable } else { Kind::Regular };
            if e.kind == kind && e.size == md.len() && e.mtime_ns == mtime_ns(&md) && e.size != 0 {
                return Ok(Some((kind, Vec::new(), md)));
            }
        }
        self.read_entry(p)
    }
}
