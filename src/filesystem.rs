//! Read-only filesystem operations, testable without a kernel FUSE connection.

use std::collections::{BTreeMap, HashMap};
use std::ffi::OsStr;
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use fuser::{
    FileAttr, FileType, ReplyAttr, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen,
    Request,
};
use jj_lib::backend::TreeValue;

use crate::projection::{Node, Projection};

pub type FsResult<T> = Result<T, i32>;

#[derive(Debug, Clone, Copy, Default)]
pub struct Stats {
    pub directory_loads: u64,
    pub blob_reads: u64,
    pub size_scans: u64,
    pub cached_bytes: usize,
}

#[derive(Debug, Clone)]
pub struct DirEntry {
    pub inode: u64,
    pub name: String,
    pub kind: FileType,
}

struct Inode {
    node: Node,
    parent: u64,
    size: Option<u64>,
}

/// A pinned inode namespace with lazy directory discovery and a byte-budgeted
/// file-content LRU. jj's own tree/backend caches are outside this budget.
pub struct ReadOnlyFs {
    projection: Projection,
    inodes: BTreeMap<u64, Inode>,
    paths: HashMap<String, u64>,
    directories: HashMap<u64, Vec<DirEntry>>,
    blobs: HashMap<u64, (Arc<Vec<u8>>, u64)>,
    budget: usize,
    clock: u64,
    uid: u32,
    gid: u32,
    stats: Stats,
}

impl ReadOnlyFs {
    pub fn new(
        projection: Projection,
        cache_bytes: usize,
        uid: u32,
        gid: u32,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            projection.root().kind() == "directory",
            "mount root is {}; select an existing directory (see inspect for boundary/conflict state)",
            projection.root().kind()
        );
        let root = Inode {
            node: projection.root().clone(),
            parent: 1,
            size: None,
        };
        Ok(Self {
            projection,
            inodes: BTreeMap::from([(1, root)]),
            paths: HashMap::new(),
            directories: HashMap::new(),
            blobs: HashMap::new(),
            budget: cache_bytes,
            clock: 0,
            uid,
            gid,
            stats: Stats::default(),
        })
    }

    pub fn projection(&self) -> &Projection {
        &self.projection
    }
    pub fn stats(&self) -> Stats {
        self.stats
    }

    fn inode(&self, ino: u64) -> FsResult<&Inode> {
        self.inodes.get(&ino).ok_or(libc::ENOENT)
    }

    fn load_dir(&mut self, ino: u64) -> FsResult<()> {
        if self.directories.contains_key(&ino) {
            return Ok(());
        }
        let inode = self.inode(ino)?;
        if inode.node.kind() != "directory" {
            return Err(libc::ENOTDIR);
        }
        let children = self.projection.children(&inode.node).map_err(io_error)?;
        let mut entries = Vec::new();
        for node in children {
            let path = node.path.as_internal_file_string().to_owned();
            let name = node
                .path
                .components()
                .last()
                .expect("child has basename")
                .as_internal_str()
                .to_owned();
            let kind = file_type(&node);
            let next = self.inodes.len() as u64 + 1;
            let child = *self.paths.entry(path).or_insert(next);
            self.inodes.entry(child).or_insert(Inode {
                node,
                parent: ino,
                size: None,
            });
            entries.push(DirEntry {
                inode: child,
                name,
                kind,
            });
        }
        self.directories.insert(ino, entries);
        self.stats.directory_loads += 1;
        Ok(())
    }

    pub fn lookup(&mut self, parent: u64, name: &OsStr) -> FsResult<u64> {
        if self.inode(parent)?.node.kind() != "directory" {
            return Err(libc::ENOTDIR);
        }
        let name = name.to_str().ok_or(libc::ENOENT)?;
        if name == "." {
            return Ok(parent);
        }
        if name == ".." {
            return Ok(self.inode(parent)?.parent);
        }
        if name.is_empty() || name.contains('/') || name.contains('\0') {
            return Err(libc::EINVAL);
        }
        self.load_dir(parent)?;
        self.directories[&parent]
            .iter()
            .find(|e| e.name == name)
            .map(|e| e.inode)
            .ok_or(libc::ENOENT)
    }

    /// `.` and `..` are included. The root's parent is itself, not a source path.
    pub fn readdir(&mut self, ino: u64) -> FsResult<Vec<DirEntry>> {
        self.load_dir(ino)?;
        let mut entries = vec![
            DirEntry {
                inode: ino,
                name: ".".into(),
                kind: FileType::Directory,
            },
            DirEntry {
                inode: self.inode(ino)?.parent,
                name: "..".into(),
                kind: FileType::Directory,
            },
        ];
        entries.extend(self.directories[&ino].clone());
        Ok(entries)
    }

    pub fn getattr(&mut self, ino: u64) -> FsResult<FileAttr> {
        let inode = self.inode(ino)?;
        let kind = file_type(&inode.node);
        let size = if let Some(size) = inode.size {
            size
        } else {
            let size = match inode.node.kind() {
                "file" => {
                    self.stats.size_scans += 1;
                    self.projection
                        .file_size(&self.inodes[&ino].node)
                        .map_err(io_error)?
                }
                "symlink" => self
                    .projection
                    .read_node(&inode.node)
                    .map_err(io_error)?
                    .len() as u64,
                _ => 0,
            };
            self.inodes.get_mut(&ino).expect("known inode").size = Some(size);
            size
        };
        let inode = self.inode(ino)?;
        let executable = matches!(
            inode.node.value.as_resolved(),
            Some(Some(TreeValue::File {
                executable: true,
                ..
            }))
        );
        let perm = match kind {
            FileType::Directory => 0o555,
            FileType::Symlink => 0o777,
            _ if executable => 0o555,
            _ => 0o444,
        };
        Ok(FileAttr {
            ino,
            size,
            blocks: size.div_ceil(512),
            atime: UNIX_EPOCH,
            mtime: UNIX_EPOCH,
            ctime: UNIX_EPOCH,
            crtime: UNIX_EPOCH,
            kind,
            perm,
            nlink: if kind == FileType::Directory { 2 } else { 1 },
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 65536,
            flags: 0,
        })
    }

    pub fn open(&self, ino: u64, flags: i32) -> FsResult<()> {
        if flags & libc::O_ACCMODE != libc::O_RDONLY
            || flags & (libc::O_TRUNC | libc::O_APPEND | libc::O_CREAT) != 0
        {
            return Err(libc::EROFS);
        }
        match self.inode(ino)?.node.kind() {
            "file" => Ok(()),
            "directory" => Err(libc::EISDIR),
            "symlink" => Err(libc::ELOOP),
            "git_submodule" => Err(libc::EOPNOTSUPP),
            _ => Err(libc::EIO),
        }
    }

    pub fn access(&self, ino: u64, mask: i32) -> FsResult<()> {
        let inode = self.inode(ino)?;
        if mask & libc::W_OK != 0 {
            return Err(libc::EROFS);
        }
        if mask & libc::X_OK != 0
            && inode.node.kind() == "file"
            && !matches!(
                inode.node.value.as_resolved(),
                Some(Some(TreeValue::File {
                    executable: true,
                    ..
                }))
            )
        {
            return Err(libc::EACCES);
        }
        Ok(())
    }

    pub fn read(&mut self, ino: u64, offset: i64, size: u32) -> FsResult<Vec<u8>> {
        if offset < 0 {
            return Err(libc::EINVAL);
        }
        self.open(ino, libc::O_RDONLY)?;
        let length = self.getattr(ino)?.size;
        if offset as u64 >= length || size == 0 {
            return Ok(Vec::new());
        }
        self.clock += 1;
        if let Some((bytes, stamp)) = self.blobs.get_mut(&ino) {
            *stamp = self.clock;
            return Ok(slice(bytes, offset as u64, size));
        }
        self.stats.blob_reads += 1;
        let node = &self.inodes[&ino].node;
        if length > self.budget as u64 {
            return self
                .projection
                .read_range(node, offset as u64, size)
                .map_err(io_error);
        }
        let bytes = Arc::new(self.projection.read_node(node).map_err(io_error)?);
        let result = slice(&bytes, offset as u64, size);
        while self.stats.cached_bytes + bytes.len() > self.budget {
            let oldest = self
                .blobs
                .iter()
                .min_by_key(|(_, (_, stamp))| *stamp)
                .map(|(ino, _)| *ino)
                .expect("cache eviction candidate");
            self.stats.cached_bytes -= self.blobs.remove(&oldest).expect("cached inode").0.len();
        }
        self.stats.cached_bytes += bytes.len();
        self.blobs.insert(ino, (bytes, self.clock));
        Ok(result)
    }

    /// Returns the recorded target without following it. Symlinks can escape a
    /// mount when followed by the host: this view is not a security sandbox.
    pub fn readlink(&self, ino: u64) -> FsResult<Vec<u8>> {
        let inode = self.inode(ino)?;
        if inode.node.kind() != "symlink" {
            return Err(libc::EINVAL);
        }
        self.projection.read_node(&inode.node).map_err(io_error)
    }
}

fn slice(bytes: &[u8], offset: u64, size: u32) -> Vec<u8> {
    let start = offset.min(bytes.len() as u64) as usize;
    bytes[start..start.saturating_add(size as usize).min(bytes.len())].to_vec()
}

fn file_type(node: &Node) -> FileType {
    match node.kind() {
        "directory" => FileType::Directory,
        "symlink" => FileType::Symlink,
        _ => FileType::RegularFile,
    }
}

fn io_error(err: anyhow::Error) -> i32 {
    eprintln!("jj-workfuse: {err:#}");
    libc::EIO
}

const TTL: Duration = Duration::from_secs(3600);

impl fuser::Filesystem for ReadOnlyFs {
    fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        match self.lookup(parent, name).and_then(|ino| self.getattr(ino)) {
            Ok(attr) => reply.entry(&TTL, &attr, 0),
            Err(err) => reply.error(err),
        }
    }
    fn getattr(&mut self, _req: &Request<'_>, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        match self.getattr(ino) {
            Ok(attr) => reply.attr(&TTL, &attr),
            Err(err) => reply.error(err),
        }
    }
    fn open(&mut self, _req: &Request<'_>, ino: u64, flags: i32, reply: ReplyOpen) {
        match ReadOnlyFs::open(self, ino, flags) {
            Ok(()) => reply.opened(ino, 0),
            Err(err) => reply.error(err),
        }
    }
    fn read(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        match self.read(ino, offset, size) {
            Ok(data) => reply.data(&data),
            Err(err) => reply.error(err),
        }
    }
    fn readlink(&mut self, _req: &Request<'_>, ino: u64, reply: ReplyData) {
        match ReadOnlyFs::readlink(self, ino) {
            Ok(data) => reply.data(&data),
            Err(err) => reply.error(err),
        }
    }
    fn opendir(&mut self, _req: &Request<'_>, ino: u64, flags: i32, reply: ReplyOpen) {
        let result = if flags & libc::O_ACCMODE != libc::O_RDONLY {
            Err(libc::EROFS)
        } else {
            self.load_dir(ino)
        };
        match result {
            Ok(()) => reply.opened(ino, 0),
            Err(err) => reply.error(err),
        }
    }
    fn readdir(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        if offset < 0 {
            reply.error(libc::EINVAL);
            return;
        }
        match self.readdir(ino) {
            Ok(entries) => {
                for (index, entry) in entries.into_iter().enumerate().skip(offset as usize) {
                    if reply.add(entry.inode, (index + 1) as i64, entry.kind, entry.name) {
                        break;
                    }
                }
                reply.ok();
            }
            Err(err) => reply.error(err),
        }
    }
    fn access(&mut self, _req: &Request<'_>, ino: u64, mask: i32, reply: ReplyEmpty) {
        match ReadOnlyFs::access(self, ino, mask) {
            Ok(()) => reply.ok(),
            Err(err) => reply.error(err),
        }
    }
}
