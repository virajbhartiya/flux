//! `flux mount`: a read-only FUSE view of a revision (Linux).
//!
//! Directories are flux trees and files are fetched on first read, so
//! mounting a huge repository is instant and only what is actually read
//! crosses the network. Everything is immutable and content-addressed: the
//! kernel may cache attributes and pages indefinitely, and a directory
//! listing fetches the directory and its immediate subdirectories in one
//! request so walking a tree costs one round trip per level.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime};

use anyhow::Result;
use bytes::Bytes;
use flux_client::Client;
use flux_core::{ContentId, EntryKind, FileKind, Tree, TreeId};
use fuser::{
    Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo, LockOwner, OpenFlags, ReplyAttr,
    ReplyData, ReplyDirectory, ReplyEntry, ReplyOpen, ReplyStatfs, Request,
};

/// Nothing ever changes under a mount.
const TTL: Duration = Duration::from_secs(365 * 24 * 3600);
/// File contents kept in memory (the kernel page cache holds more).
const CONTENT_CACHE_BYTES: usize = 512 << 20;

#[derive(Clone)]
enum NodeKind {
    Dir(TreeId),
    File {
        content: ContentId,
        size: u64,
        exec: bool,
    },
    Link {
        content: ContentId,
        size: u64,
    },
    /// A submodule pointer: shown as an empty directory.
    GitLink,
}

struct Node {
    kind: NodeKind,
    parent: u64,
}

pub struct FluxFs {
    rt: tokio::runtime::Handle,
    client: Client,
    /// Inode `i` is `nodes[i - 1]`.
    nodes: RwLock<Vec<Node>>,
    /// Loaded directories: inode → sorted `(name, inode)`.
    children: RwLock<HashMap<u64, Arc<Vec<(Vec<u8>, u64)>>>>,
    trees: Mutex<HashMap<TreeId, Arc<Tree>>>,
    contents: Mutex<ContentCache>,
    uid: u32,
    gid: u32,
    mtime: SystemTime,
}

#[derive(Default)]
struct ContentCache {
    map: HashMap<ContentId, Bytes>,
    order: std::collections::VecDeque<ContentId>,
    bytes: usize,
}

impl ContentCache {
    fn get(&self, id: &ContentId) -> Option<Bytes> {
        self.map.get(id).cloned()
    }

    fn insert(&mut self, id: ContentId, data: Bytes) {
        if self.map.contains_key(&id) {
            return;
        }
        self.bytes += data.len();
        self.map.insert(id, data);
        self.order.push_back(id);
        while self.bytes > CONTENT_CACHE_BYTES
            && let Some(old) = self.order.pop_front()
        {
            if let Some(d) = self.map.remove(&old) {
                self.bytes -= d.len();
            }
        }
    }
}

impl FluxFs {
    pub fn new(rt: tokio::runtime::Handle, client: Client, root: TreeId, mtime: SystemTime) -> Self {
        // SAFETY: getuid/getgid have no preconditions.
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        Self {
            rt,
            client,
            nodes: RwLock::new(vec![Node { kind: NodeKind::Dir(root), parent: 1 }]),
            children: Default::default(),
            trees: Default::default(),
            contents: Default::default(),
            uid,
            gid,
            mtime,
        }
    }

    fn node_kind(&self, ino: u64) -> Option<NodeKind> {
        self.nodes.read().unwrap().get(ino.checked_sub(1)? as usize).map(|n| n.kind.clone())
    }

    fn tree(&self, id: TreeId) -> Result<Arc<Tree>> {
        if let Some(t) = self.trees.lock().unwrap().get(&id) {
            return Ok(t.clone());
        }
        // The directory and its immediate subdirectories in one request.
        let got = self.rt.block_on(self.client.get_trees(&[id], 1, &[], b""))?;
        let mut trees = self.trees.lock().unwrap();
        for (tid, _, t) in got {
            trees.entry(tid).or_insert_with(|| Arc::new(t));
        }
        trees.get(&id).cloned().ok_or_else(|| anyhow::anyhow!("tree {id} not returned"))
    }

    /// The children of a directory inode, loading them on first use.
    fn children(&self, ino: u64) -> Result<Option<Arc<Vec<(Vec<u8>, u64)>>>, Errno> {
        if let Some(c) = self.children.read().unwrap().get(&ino) {
            return Ok(Some(c.clone()));
        }
        let tree_id = match self.node_kind(ino) {
            Some(NodeKind::Dir(t)) => t,
            Some(NodeKind::GitLink) => return Ok(Some(Arc::new(Vec::new()))),
            Some(_) => return Ok(None),
            None => return Err(Errno::ENOENT),
        };
        let tree = self.tree(tree_id).map_err(|e| {
            eprintln!("flux mount: {e:#}");
            Errno::EIO
        })?;
        let mut children = self.children.write().unwrap();
        if let Some(c) = children.get(&ino) {
            return Ok(Some(c.clone())); // another thread got here first
        }
        let mut nodes = self.nodes.write().unwrap();
        let mut list = Vec::with_capacity(tree.entries().len());
        for e in tree.entries() {
            let kind = match e.kind {
                EntryKind::Dir => NodeKind::Dir(e.tree_id().expect("dir entry")),
                EntryKind::File(FileKind::GitLink(_)) => NodeKind::GitLink,
                EntryKind::File(FileKind::Symlink) => NodeKind::Link { content: e.content_id().expect("file"), size: e.size },
                EntryKind::File(k) => {
                    NodeKind::File { content: e.content_id().expect("file"), size: e.size, exec: k.is_executable() }
                }
            };
            nodes.push(Node { kind, parent: ino });
            list.push((e.name.as_bytes().to_vec(), nodes.len() as u64));
        }
        let list = Arc::new(list);
        children.insert(ino, list.clone());
        Ok(Some(list))
    }

    fn attr(&self, ino: u64, kind: &NodeKind) -> FileAttr {
        let (ft, size, perm, nlink) = match kind {
            NodeKind::Dir(_) | NodeKind::GitLink => (FileType::Directory, 0, 0o555, 2),
            NodeKind::File { size, exec, .. } => (FileType::RegularFile, *size, if *exec { 0o555 } else { 0o444 }, 1),
            NodeKind::Link { size, .. } => (FileType::Symlink, *size, 0o777, 1),
        };
        FileAttr {
            ino: INodeNo(ino),
            size,
            blocks: size.div_ceil(512),
            atime: self.mtime,
            mtime: self.mtime,
            ctime: self.mtime,
            crtime: self.mtime,
            kind: ft,
            perm,
            nlink,
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            flags: 0,
            blksize: 4096,
        }
    }

    fn content(&self, id: ContentId) -> Result<Bytes, Errno> {
        if let Some(b) = self.contents.lock().unwrap().get(&id) {
            return Ok(b);
        }
        let mut data = None;
        self.rt
            .block_on(self.client.get_contents(&[id], |_, d| {
                data = Some(Bytes::from(d));
                Ok(())
            }))
            .map_err(|e| {
                eprintln!("flux mount: {e:#}");
                Errno::EIO
            })?;
        let data = data.ok_or(Errno::EIO)?;
        self.contents.lock().unwrap().insert(id, data.clone());
        Ok(data)
    }
}

impl Filesystem for FluxFs {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let list = match self.children(parent.0) {
            Ok(Some(l)) => l,
            Ok(None) => return reply.error(Errno::ENOTDIR),
            Err(e) => return reply.error(e),
        };
        match list.binary_search_by(|(n, _)| n.as_slice().cmp(name.as_bytes())) {
            Ok(i) => {
                let ino = list[i].1;
                let kind = self.node_kind(ino).expect("child inode exists");
                reply.entry(&TTL, &self.attr(ino, &kind), Generation(0));
            }
            Err(_) => reply.error(Errno::ENOENT),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        match self.node_kind(ino.0) {
            Some(kind) => reply.attr(&TTL, &self.attr(ino.0, &kind)),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        match self.node_kind(ino.0) {
            Some(NodeKind::Link { content, .. }) => match self.content(content) {
                Ok(d) => reply.data(&d),
                Err(e) => reply.error(e),
            },
            _ => reply.error(Errno::EINVAL),
        }
    }

    fn open(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        match self.node_kind(ino.0) {
            // Immutable content: let the kernel keep its page cache across opens.
            Some(NodeKind::File { .. }) => reply.opened(FileHandle(0), FopenFlags::FOPEN_KEEP_CACHE),
            Some(_) => reply.error(Errno::EISDIR),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let Some(NodeKind::File { content, .. }) = self.node_kind(ino.0) else {
            return reply.error(Errno::EISDIR);
        };
        match self.content(content) {
            Ok(d) => {
                let start = (offset as usize).min(d.len());
                let end = start.saturating_add(size as usize).min(d.len());
                reply.data(&d[start..end]);
            }
            Err(e) => reply.error(e),
        }
    }

    fn readdir(&self, _req: &Request, ino: INodeNo, _fh: FileHandle, offset: u64, mut reply: ReplyDirectory) {
        let list = match self.children(ino.0) {
            Ok(Some(l)) => l,
            Ok(None) => return reply.error(Errno::ENOTDIR),
            Err(e) => return reply.error(e),
        };
        let parent = self.nodes.read().unwrap()[ino.0 as usize - 1].parent;
        let mut entries: Vec<(u64, FileType, &[u8])> =
            vec![(ino.0, FileType::Directory, b"."), (parent, FileType::Directory, b"..")];
        let nodes = self.nodes.read().unwrap();
        for (name, child) in list.iter() {
            let ft = match nodes[*child as usize - 1].kind {
                NodeKind::Dir(_) | NodeKind::GitLink => FileType::Directory,
                NodeKind::File { .. } => FileType::RegularFile,
                NodeKind::Link { .. } => FileType::Symlink,
            };
            entries.push((*child, ft, name.as_slice()));
        }
        for (i, (child, ft, name)) in entries.into_iter().enumerate().skip(offset as usize) {
            if reply.add(INodeNo(child), (i + 1) as u64, ft, OsStr::from_bytes(name)) {
                break;
            }
        }
        reply.ok();
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        let files = self.nodes.read().unwrap().len() as u64;
        reply.statfs(0, 0, 0, files, 0, 4096, 4096, 4096);
    }
}

/// Mount `root` at `dir` until Ctrl-C (or `umount`), then unmount.
pub async fn mount(client: Client, root: TreeId, mtime: SystemTime, dir: &std::path::Path) -> Result<()> {
    let fs = FluxFs::new(tokio::runtime::Handle::current(), client, root, mtime);
    let mut cfg = fuser::Config::default();
    cfg.mount_options.extend([
        fuser::MountOption::RO,
        fuser::MountOption::FSName("flux".into()),
        fuser::MountOption::DefaultPermissions,
    ]);
    cfg.n_threads = Some(8);
    let session = fuser::spawn_mount(fs, dir, &cfg)?;
    tokio::signal::ctrl_c().await?;
    session.umount_and_join()?;
    Ok(())
}
