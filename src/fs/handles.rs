use dashmap::DashMap;
use std::os::unix::io::RawFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Clone)]
pub struct HandleState {
    /// The file handle ID assigned by dibs.
    pub fh: u64,
    /// The real file descriptor in the backing filesystem.
    pub real_fd: RawFd,
    /// Path relative to backing root.
    pub path: PathBuf,
    /// SHA-256 or xxHash at the time this handle was opened.
    pub hash_at_open: Option<Vec<u8>>,
    /// Open flags.
    pub flags: i32,
    /// Whether this handle has been used for writing.
    pub has_written: bool,
    /// Session ID of the process that opened this handle.
    pub sid: u32,
}

pub struct HandleTable {
    handles: DashMap<u64, HandleState>,
    next_fh: AtomicU64,
}

impl HandleTable {
    pub fn new() -> Self {
        Self {
            handles: DashMap::new(),
            next_fh: AtomicU64::new(1),
        }
    }

    pub fn alloc(&self, real_fd: RawFd, path: PathBuf, flags: i32, hash: Option<Vec<u8>>, sid: u32) -> u64 {
        let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
        let state = HandleState {
            fh,
            real_fd,
            path,
            hash_at_open: hash,
            flags,
            has_written: false,
            sid,
        };
        self.handles.insert(fh, state);
        fh
    }

    pub fn get(&self, fh: u64) -> Option<dashmap::mapref::one::Ref<'_, u64, HandleState>> {
        self.handles.get(&fh)
    }

    pub fn get_mut(&self, fh: u64) -> Option<dashmap::mapref::one::RefMut<'_, u64, HandleState>> {
        self.handles.get_mut(&fh)
    }

    pub fn remove(&self, fh: u64) -> Option<HandleState> {
        self.handles.remove(&fh).map(|(_, v)| v)
    }
}

/// State for directory handles.
#[derive(Debug)]
pub struct DirHandleState {
    pub fh: u64,
    pub real_fd: RawFd,
    pub path: PathBuf,
}

pub struct DirHandleTable {
    handles: DashMap<u64, DirHandleState>,
    next_fh: AtomicU64,
}

impl DirHandleTable {
    pub fn new() -> Self {
        Self {
            handles: DashMap::new(),
            next_fh: AtomicU64::new(1),
        }
    }

    pub fn alloc(&self, real_fd: RawFd, path: PathBuf) -> u64 {
        let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
        self.handles.insert(
            fh,
            DirHandleState {
                fh,
                real_fd,
                path,
            },
        );
        fh
    }

    pub fn get(&self, fh: u64) -> Option<dashmap::mapref::one::Ref<'_, u64, DirHandleState>> {
        self.handles.get(&fh)
    }

    pub fn remove(&self, fh: u64) -> Option<DirHandleState> {
        self.handles.remove(&fh).map(|(_, v)| v)
    }
}
