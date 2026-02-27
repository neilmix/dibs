pub mod cas;
pub mod handles;
pub mod inodes;
pub mod passthrough;
pub mod virtual_dir;

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dashmap::{DashMap, DashSet};
use fuser::{
    AccessFlags, BsdFileFlags, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags,
    Generation, INodeNo, KernelConfig, LockOwner, OpenFlags, ReplyAttr, ReplyCreate, ReplyData,
    ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, RenameFlags,
    Request, TimeOrNow, WriteFlags,
};
use parking_lot::Mutex;
use tracing::{debug, info, warn};

use self::handles::{DirHandleTable, HandleTable};
use self::inodes::*;
use self::passthrough::*;
use self::virtual_dir::*;
use crate::config::DibsConfig;
use crate::state::hash_table::CasTable;

const TTL: Duration = Duration::from_secs(1);

/// Get the session ID for a given PID. Falls back to the PID itself on error.
fn get_sid(pid: u32) -> u32 {
    let sid = unsafe { libc::getsid(pid as i32) };
    if sid < 0 { pid } else { sid as u32 }
}

pub struct DibsFs {
    pub config: DibsConfig,
    /// The backing directory root.
    pub backing: PathBuf,
    /// Inode table mapping inodes <-> paths (relative to backing root).
    pub inodes: InodeTable,
    /// File handle table.
    pub file_handles: HandleTable,
    /// Directory handle table.
    pub dir_handles: DirHandleTable,
    /// CAS tracking table.
    pub cas_table: Arc<CasTable>,
    /// Start time for uptime reporting.
    pub start_time: std::time::Instant,
    /// Expected writes for self-write suppression (used by watcher).
    pub expected_writes: Arc<DashSet<PathBuf>>,
    /// Recently flushed self-writes — third suppression layer for delayed
    /// watcher events that arrive after expected_writes has been consumed
    /// and the write owner released. Entries expire after 2 seconds.
    pub recent_self_writes: Arc<DashMap<PathBuf, std::time::Instant>>,
    /// Watcher handle — kept alive for the lifetime of the filesystem.
    pub watcher: Mutex<Option<notify::RecommendedWatcher>>,
    /// Conflict storage directory in the backing fs.
    pub conflict_dir: Option<PathBuf>,
}

impl DibsFs {
    pub fn new(config: DibsConfig) -> Self {
        let backing = config.backing.clone();
        let conflict_dir = if config.save_conflicts {
            let dir = config.backing.join(".dibs-conflicts");
            let _ = std::fs::create_dir_all(&dir);
            Some(dir)
        } else {
            None
        };

        Self {
            config,
            backing,
            inodes: InodeTable::new(),
            file_handles: HandleTable::new(),
            dir_handles: DirHandleTable::new(),
            cas_table: Arc::new(CasTable::new()),
            start_time: std::time::Instant::now(),
            expected_writes: Arc::new(DashSet::new()),
            recent_self_writes: Arc::new(DashMap::new()),
            watcher: Mutex::new(None),
            conflict_dir,
        }
    }

    /// Convert a relative path (from inode table) to full backing path.
    fn backing_path(&self, rel: &Path) -> PathBuf {
        self.backing.join(rel)
    }

    /// Convert a backing inode to its relative path, resolving via lookup if needed.
    fn resolve_path(&self, parent: u64, name: &OsStr) -> (PathBuf, PathBuf) {
        let parent_rel = if parent == 1 {
            PathBuf::new()
        } else {
            self.inodes.get_path(parent).unwrap_or_default()
        };
        let rel = parent_rel.join(name);
        let full = self.backing_path(&rel);
        (rel, full)
    }

    /// Stat a path and register in inode table.
    fn lookup_and_register(&self, rel: &Path, full: &Path) -> std::io::Result<FileAttr> {
        let st = lstat(full)?;
        let mut attr = stat_to_file_attr(&st);
        // For the root directory, force inode to 1
        if rel.as_os_str().is_empty() {
            attr.ino = INodeNo(1);
            self.inodes.insert(1, PathBuf::new());
        } else {
            self.inodes.insert(u64::from(attr.ino), rel.to_path_buf());
        }
        Ok(attr)
    }

    /// Check if a name refers to the virtual .dibs directory.
    fn is_dibs_name(name: &OsStr) -> bool {
        name.as_bytes() == DIBS_DIR_NAME.as_bytes()
    }

    /// Check if an inode is in the virtual .dibs space.
    fn is_dibs_ino(ino: u64) -> bool {
        InodeTable::is_synthetic(ino)
    }

    /// Build a synthetic FileAttr for a virtual directory.
    fn dibs_dir_attr(ino: u64) -> FileAttr {
        FileAttr {
            ino: INodeNo(ino),
            size: 0,
            blocks: 0,
            atime: SystemTime::now(),
            mtime: SystemTime::now(),
            ctime: SystemTime::now(),
            crtime: SystemTime::now(),
            kind: FileType::Directory,
            perm: 0o555,
            nlink: 2,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            rdev: 0,
            blksize: 512,
            flags: 0,
        }
    }

    /// Build a synthetic FileAttr for a virtual file.
    fn dibs_file_attr(ino: u64, size: u64) -> FileAttr {
        FileAttr {
            ino: INodeNo(ino),
            size,
            blocks: (size + 511) / 512,
            atime: SystemTime::now(),
            mtime: SystemTime::now(),
            ctime: SystemTime::now(),
            crtime: SystemTime::now(),
            kind: FileType::RegularFile,
            perm: 0o444,
            nlink: 1,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            rdev: 0,
            blksize: 512,
            flags: 0,
        }
    }

    /// Generate status JSON.
    fn status_json(&self) -> String {
        let uptime = self.start_time.elapsed().as_secs();
        let tracked = self.cas_table.len();
        let active_locks = self.cas_table.active_writers();
        serde_json::json!({
            "tracked_files": tracked,
            "active_locks": active_locks,
            "uptime_seconds": uptime,
            "session_id": self.config.session_id,
        })
        .to_string()
    }

    /// Generate locks JSON.
    fn locks_json(&self) -> String {
        let entries = self.cas_table.all_entries();
        serde_json::to_string_pretty(&entries).unwrap_or_else(|_| "[]".to_string())
    }
}

impl Filesystem for DibsFs {
    fn init(
        &mut self,
        _req: &Request,
        _config: &mut KernelConfig,
    ) -> std::io::Result<()> {
        info!("dibs filesystem initialized, backing={}", self.backing.display());

        // Register root inode
        self.inodes.insert(1, PathBuf::new());

        // Start file watcher
        crate::watcher::start_watcher(self);

        Ok(())
    }

    fn destroy(&mut self) {
        info!("dibs filesystem shutting down");
        let mut w = self.watcher.lock();
        *w = None;
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let parent = u64::from(parent);
        debug!("lookup(parent={}, name={:?})", parent, name);

        // Virtual .dibs/ directory at root
        if parent == 1 && Self::is_dibs_name(name) {
            reply.entry(&TTL, &Self::dibs_dir_attr(DIBS_DIR_INO), Generation(0));
            return;
        }

        // Virtual .dibs/ children
        if parent == DIBS_DIR_INO {
            let name_bytes = name.as_bytes();
            if name_bytes == DIBS_STATUS_NAME.as_bytes() {
                let content = self.status_json();
                reply.entry(&TTL, &Self::dibs_file_attr(DIBS_STATUS_INO, content.len() as u64), Generation(0));
                return;
            }
            if name_bytes == DIBS_LOCKS_NAME.as_bytes() {
                let content = self.locks_json();
                reply.entry(&TTL, &Self::dibs_file_attr(DIBS_LOCKS_INO, content.len() as u64), Generation(0));
                return;
            }
            if name_bytes == DIBS_CONFLICTS_NAME.as_bytes() {
                reply.entry(&TTL, &Self::dibs_dir_attr(DIBS_CONFLICTS_DIR_INO), Generation(0));
                return;
            }
            reply.error(Errno::ENOENT);
            return;
        }

        let (rel, full) = self.resolve_path(parent, name);
        match self.lookup_and_register(&rel, &full) {
            Ok(attr) => reply.entry(&TTL, &attr, Generation(0)),
            Err(e) => reply.error(Errno::from(e)),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let ino = u64::from(ino);
        debug!("getattr(ino={})", ino);

        // Root inode
        if ino == 1 {
            match lstat(&self.backing) {
                Ok(st) => {
                    let mut attr = stat_to_file_attr(&st);
                    attr.ino = INodeNo(1);
                    reply.attr(&TTL, &attr);
                }
                Err(e) => reply.error(Errno::from(e)),
            }
            return;
        }

        // Virtual inodes
        if ino == DIBS_DIR_INO {
            reply.attr(&TTL, &Self::dibs_dir_attr(DIBS_DIR_INO));
            return;
        }
        if ino == DIBS_STATUS_INO {
            let content = self.status_json();
            reply.attr(&TTL, &Self::dibs_file_attr(DIBS_STATUS_INO, content.len() as u64));
            return;
        }
        if ino == DIBS_LOCKS_INO {
            let content = self.locks_json();
            reply.attr(&TTL, &Self::dibs_file_attr(DIBS_LOCKS_INO, content.len() as u64));
            return;
        }
        if ino == DIBS_CONFLICTS_DIR_INO {
            reply.attr(&TTL, &Self::dibs_dir_attr(DIBS_CONFLICTS_DIR_INO));
            return;
        }

        // Real inode
        if let Some(rel) = self.inodes.get_path(ino) {
            let full = self.backing_path(&rel);
            match lstat(&full) {
                Ok(st) => {
                    let mut attr = stat_to_file_attr(&st);
                    attr.ino = INodeNo(ino);
                    reply.attr(&TTL, &attr);
                }
                Err(e) => reply.error(Errno::from(e)),
            }
        } else {
            reply.error(Errno::ENOENT);
        }
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        flags: Option<BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let ino = u64::from(ino);
        debug!("setattr(ino={})", ino);

        if Self::is_dibs_ino(ino) {
            reply.error(Errno::EACCES);
            return;
        }

        let rel = match self.inodes.get_path(ino) {
            Some(p) => p,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        let full = self.backing_path(&rel);
        let c_path = match path_to_cstring(&full) {
            Ok(p) => p,
            Err(_) => {
                reply.error(Errno::EINVAL);
                return;
            }
        };

        // Handle truncate — needs CAS check
        if let Some(new_size) = size {
            if let Some(handle_fh) = fh {
                let handle_fh = u64::from(handle_fh);
                let sid = self.file_handles.get(handle_fh).map(|h| h.sid).unwrap_or(0);
                if let Err(e) = self.cas_table.check_and_acquire_write(&rel, handle_fh, sid, &self.file_handles) {
                    warn!("CAS conflict on truncate: {}", e);
                    reply.error(Errno::EIO);
                    return;
                }
            }
            let fd = if let Some(handle_fh) = fh {
                self.file_handles.get(u64::from(handle_fh)).map(|h| h.real_fd)
            } else {
                None
            };
            let rc = if let Some(fd) = fd {
                unsafe { libc::ftruncate(fd, new_size as libc::off_t) }
            } else {
                unsafe { libc::truncate(c_path.as_ptr(), new_size as libc::off_t) }
            };
            if rc != 0 {
                reply.error(Errno::from(std::io::Error::last_os_error()));
                return;
            }
        }

        if let Some(mode) = mode {
            unsafe {
                if libc::chmod(c_path.as_ptr(), mode as libc::mode_t) != 0 {
                    reply.error(Errno::from(std::io::Error::last_os_error()));
                    return;
                }
            }
        }

        if uid.is_some() || gid.is_some() {
            let new_uid = uid.map(|u| u as libc::uid_t).unwrap_or(u32::MAX);
            let new_gid = gid.map(|g| g as libc::gid_t).unwrap_or(u32::MAX);
            unsafe {
                if libc::chown(c_path.as_ptr(), new_uid, new_gid) != 0 {
                    reply.error(Errno::from(std::io::Error::last_os_error()));
                    return;
                }
            }
        }

        if atime.is_some() || mtime.is_some() {
            let to_timespec = |t: Option<TimeOrNow>| -> libc::timespec {
                match t {
                    Some(TimeOrNow::SpecificTime(st)) => {
                        let d = st.duration_since(UNIX_EPOCH).unwrap_or_default();
                        libc::timespec {
                            tv_sec: d.as_secs() as libc::time_t,
                            tv_nsec: d.subsec_nanos() as libc::c_long,
                        }
                    }
                    Some(TimeOrNow::Now) => libc::timespec {
                        tv_sec: 0,
                        tv_nsec: libc::UTIME_NOW,
                    },
                    None => libc::timespec {
                        tv_sec: 0,
                        tv_nsec: libc::UTIME_OMIT,
                    },
                }
            };
            let times = [to_timespec(atime), to_timespec(mtime)];
            unsafe {
                if libc::utimensat(libc::AT_FDCWD, c_path.as_ptr(), times.as_ptr(), 0) != 0 {
                    reply.error(Errno::from(std::io::Error::last_os_error()));
                    return;
                }
            }
        }

        if let Some(flags) = flags {
            unsafe {
                if libc::chflags(c_path.as_ptr(), flags.bits()) != 0 {
                    reply.error(Errno::from(std::io::Error::last_os_error()));
                    return;
                }
            }
        }

        // Return updated attrs
        match lstat(&full) {
            Ok(st) => {
                let mut attr = stat_to_file_attr(&st);
                attr.ino = INodeNo(ino);
                reply.attr(&TTL, &attr);
            }
            Err(e) => reply.error(Errno::from(e)),
        }
    }

    fn open(&self, req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let ino = u64::from(ino);
        let raw_flags = flags.0;
        debug!("open(ino={}, flags={})", ino, raw_flags);

        // Virtual files
        if ino == DIBS_STATUS_INO || ino == DIBS_LOCKS_INO {
            let fh = self.file_handles.alloc(-1, PathBuf::from(".dibs/virtual"), raw_flags, None, 0);
            reply.opened(FileHandle(fh), FopenFlags::empty());
            return;
        }
        if Self::is_dibs_ino(ino) {
            reply.error(Errno::EACCES);
            return;
        }

        let rel = match self.inodes.get_path(ino) {
            Some(p) => p,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        let full = self.backing_path(&rel);
        let c_path = match path_to_cstring(&full) {
            Ok(p) => p,
            Err(_) => {
                reply.error(Errno::EINVAL);
                return;
            }
        };

        let access_mode = raw_flags & libc::O_ACCMODE;

        // For write-mode opens that may truncate the file, suppress watcher
        // events BEFORE libc::open (which does the actual truncation).
        if access_mode != libc::O_RDONLY {
            self.expected_writes.insert(full.clone());
            self.recent_self_writes.insert(full.clone(), std::time::Instant::now());
        }

        let fd = unsafe { libc::open(c_path.as_ptr(), raw_flags) };
        if fd < 0 {
            if access_mode != libc::O_RDONLY {
                self.expected_writes.remove(&full);
                self.recent_self_writes.remove(&full);
            }
            reply.error(Errno::from(std::io::Error::last_os_error()));
            return;
        }

        let sid = get_sid(req.pid());

        let hash = if access_mode == libc::O_WRONLY {
            // Write-only: ensure CAS entry exists but don't update hash
            self.cas_table.record_write_open(&rel);
            debug!("open: write-only {} sid={}", rel.display(), sid);
            None
        } else {
            // O_RDONLY or O_RDWR: compute hash, record in CAS and reader_hashes
            let h = cas::hash_file(&full).ok();
            if let Some(ref h) = h {
                self.cas_table.record_read_open(&rel, h.clone(), sid);
                debug!("open: tracked {} hash={} sid={}", rel.display(), cas::hash_hex(h), sid);
            }
            h
        };

        let fh = self.file_handles.alloc(fd, rel, raw_flags, hash, sid);
        reply.opened(FileHandle(fh), FopenFlags::empty());
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let ino = u64::from(ino);
        let fh = u64::from(fh);
        debug!("read(ino={}, fh={}, offset={}, size={})", ino, fh, offset, size);

        // Virtual status file
        if ino == DIBS_STATUS_INO {
            let content = self.status_json();
            let bytes = content.as_bytes();
            let start = offset as usize;
            if start >= bytes.len() {
                reply.data(&[]);
            } else {
                let end = std::cmp::min(start + size as usize, bytes.len());
                reply.data(&bytes[start..end]);
            }
            return;
        }

        // Virtual locks file
        if ino == DIBS_LOCKS_INO {
            let content = self.locks_json();
            let bytes = content.as_bytes();
            let start = offset as usize;
            if start >= bytes.len() {
                reply.data(&[]);
            } else {
                let end = std::cmp::min(start + size as usize, bytes.len());
                reply.data(&bytes[start..end]);
            }
            return;
        }

        let handle = match self.file_handles.get(fh) {
            Some(h) => h,
            None => {
                reply.error(Errno::EBADF);
                return;
            }
        };

        let mut buf = vec![0u8; size as usize];
        let n = unsafe { libc::pread(handle.real_fd, buf.as_mut_ptr() as *mut libc::c_void, size as usize, offset as libc::off_t) };
        if n < 0 {
            reply.error(Errno::from(std::io::Error::last_os_error()));
        } else {
            buf.truncate(n as usize);
            reply.data(&buf);
        }
    }

    fn write(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        let ino = u64::from(ino);
        let fh = u64::from(fh);
        debug!("write(ino={}, fh={}, offset={}, size={})", ino, fh, offset, data.len());

        if Self::is_dibs_ino(ino) {
            reply.error(Errno::EACCES);
            return;
        }

        // Get the handle's path and SID for CAS check
        let (real_fd, rel_path, sid) = match self.file_handles.get(fh) {
            Some(h) => (h.real_fd, h.path.clone(), h.sid),
            None => {
                reply.error(Errno::EBADF);
                return;
            }
        };

        // CAS check — first write from this handle does the check
        if let Err(e) = self.cas_table.check_and_acquire_write(&rel_path, fh, sid, &self.file_handles) {
            warn!("CAS conflict on write: {}", e);

            // Save conflict data if configured
            if let Some(ref conflict_dir) = self.conflict_dir {
                let ts = chrono::Utc::now().format("%Y%m%d_%H%M%S_%3f");
                let fname = rel_path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "unknown".to_string());
                let conflict_path = conflict_dir.join(format!("{}_{}", ts, fname));
                let _ = std::fs::write(&conflict_path, data);
            }

            reply.error(Errno::EIO);
            return;
        }

        // Mark handle as having written
        if let Some(mut h) = self.file_handles.get_mut(fh) {
            h.has_written = true;
        }

        // Mark expected write for watcher suppression
        let full = self.backing_path(&rel_path);
        self.expected_writes.insert(full.clone());

        let n = unsafe {
            libc::pwrite(real_fd, data.as_ptr() as *const libc::c_void, data.len(), offset as libc::off_t)
        };

        if n < 0 {
            self.expected_writes.remove(&full);
            reply.error(Errno::from(std::io::Error::last_os_error()));
        } else {
            reply.written(n as u32);
        }
    }

    fn flush(&self, _req: &Request, ino: INodeNo, fh: FileHandle, _lock_owner: LockOwner, reply: ReplyEmpty) {
        let ino = u64::from(ino);
        let fh = u64::from(fh);
        debug!("flush(ino={}, fh={})", ino, fh);

        if Self::is_dibs_ino(ino) {
            reply.ok();
            return;
        }

        let (has_written, rel_path, sid) = match self.file_handles.get(fh) {
            Some(h) => (h.has_written, h.path.clone(), h.sid),
            None => {
                reply.ok();
                return;
            }
        };

        if has_written {
            // Update the hash in the CAS table
            let full = self.backing_path(&rel_path);
            if let Ok(new_hash) = cas::hash_file(&full) {
                self.cas_table.update_hash(&rel_path, new_hash.clone());
                // Update reader hash for this SID
                self.cas_table.update_reader(sid, &rel_path, new_hash.clone());
                // Update the handle's hash for future checks
                if let Some(mut h) = self.file_handles.get_mut(fh) {
                    h.hash_at_open = Some(new_hash);
                    h.has_written = false;
                }
                debug!("flush: updated hash for {} sid={}", rel_path.display(), sid);
            }
            // Release write ownership
            self.cas_table.release_write(&rel_path, fh);
            // Clear expected write
            self.expected_writes.remove(&full);
            // Record for delayed watcher event suppression (Layer 3)
            self.recent_self_writes.insert(full, std::time::Instant::now());
        }

        reply.ok();
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        let fh = u64::from(fh);
        debug!("release(fh={})", fh);

        if let Some(handle) = self.file_handles.remove(fh) {
            self.cas_table.release_write(&handle.path, fh);

            if handle.real_fd >= 0 {
                unsafe {
                    libc::close(handle.real_fd);
                }
            }
        }
        reply.ok();
    }

    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        let ino = u64::from(ino);
        debug!("opendir(ino={})", ino);

        // Virtual .dibs/ directory
        if ino == DIBS_DIR_INO || ino == DIBS_CONFLICTS_DIR_INO {
            let fh = self.dir_handles.alloc(-1, PathBuf::from(".dibs"));
            reply.opened(FileHandle(fh), FopenFlags::empty());
            return;
        }

        let rel = if ino == 1 {
            PathBuf::new()
        } else {
            match self.inodes.get_path(ino) {
                Some(p) => p,
                None => {
                    reply.error(Errno::ENOENT);
                    return;
                }
            }
        };

        let full = self.backing_path(&rel);
        let c_path = match path_to_cstring(&full) {
            Ok(p) => p,
            Err(_) => {
                reply.error(Errno::EINVAL);
                return;
            }
        };

        let dp = unsafe { libc::opendir(c_path.as_ptr()) };
        if dp.is_null() {
            reply.error(Errno::from(std::io::Error::last_os_error()));
            return;
        }

        let fd = unsafe { libc::dirfd(dp) };
        let real_fd = unsafe { libc::dup(fd) };
        unsafe {
            libc::closedir(dp);
        }

        let fh = self.dir_handles.alloc(real_fd, rel);
        reply.opened(FileHandle(fh), FopenFlags::empty());
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let ino = u64::from(ino);
        debug!("readdir(ino={}, offset={})", ino, offset);

        // Virtual .dibs/ directory
        if ino == DIBS_DIR_INO {
            let entries = vec![
                (DIBS_DIR_INO, FileType::Directory, "."),
                (1, FileType::Directory, ".."),
                (DIBS_STATUS_INO, FileType::RegularFile, DIBS_STATUS_NAME),
                (DIBS_LOCKS_INO, FileType::RegularFile, DIBS_LOCKS_NAME),
                (DIBS_CONFLICTS_DIR_INO, FileType::Directory, DIBS_CONFLICTS_NAME),
            ];
            for (i, (ino, kind, name)) in entries.iter().enumerate().skip(offset as usize) {
                if reply.add(INodeNo(*ino), (i + 1) as u64, *kind, name) {
                    break;
                }
            }
            reply.ok();
            return;
        }

        if ino == DIBS_CONFLICTS_DIR_INO {
            let entries = vec![
                (DIBS_CONFLICTS_DIR_INO, FileType::Directory, "."),
                (DIBS_DIR_INO, FileType::Directory, ".."),
            ];
            for (i, (ino, kind, name)) in entries.iter().enumerate().skip(offset as usize) {
                if reply.add(INodeNo(*ino), (i + 1) as u64, *kind, name) {
                    break;
                }
            }
            reply.ok();
            return;
        }

        // Real directory
        let rel = if ino == 1 {
            PathBuf::new()
        } else {
            match self.inodes.get_path(ino) {
                Some(p) => p,
                None => {
                    reply.error(Errno::ENOENT);
                    return;
                }
            }
        };

        let full = self.backing_path(&rel);
        let entries = match std::fs::read_dir(&full) {
            Ok(rd) => rd,
            Err(e) => {
                reply.error(Errno::from(e));
                return;
            }
        };

        let mut all_entries: Vec<(u64, FileType, String)> = Vec::new();
        // Add . and ..
        all_entries.push((ino, FileType::Directory, ".".to_string()));
        let parent_ino = if ino == 1 {
            1
        } else {
            let parent_path = rel.parent().map(|p| p.to_path_buf()).unwrap_or_default();
            if parent_path.as_os_str().is_empty() {
                1
            } else {
                self.inodes.get_ino(&parent_path).unwrap_or(1)
            }
        };
        all_entries.push((parent_ino, FileType::Directory, "..".to_string()));

        // Add .dibs at root level
        if ino == 1 {
            all_entries.push((DIBS_DIR_INO, FileType::Directory, DIBS_DIR_NAME.to_string()));
        }

        for entry in entries {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            let name = entry.file_name().to_string_lossy().to_string();
            // Skip .dibs-conflicts internal directory
            if name == ".dibs-conflicts" {
                continue;
            }

            let child_rel = rel.join(&name);
            let child_full = self.backing_path(&child_rel);
            if let Ok(st) = lstat(&child_full) {
                let attr = stat_to_file_attr(&st);
                self.inodes.insert(u64::from(attr.ino), child_rel);
                all_entries.push((u64::from(attr.ino), attr.kind, name));
            }
        }

        for (i, (entry_ino, kind, name)) in all_entries.iter().enumerate().skip(offset as usize) {
            if reply.add(INodeNo(*entry_ino), (i + 1) as u64, *kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn releasedir(&self, _req: &Request, _ino: INodeNo, fh: FileHandle, _flags: OpenFlags, reply: ReplyEmpty) {
        let fh = u64::from(fh);
        debug!("releasedir(fh={})", fh);
        if let Some(handle) = self.dir_handles.remove(fh) {
            if handle.real_fd >= 0 {
                unsafe {
                    libc::close(handle.real_fd);
                }
            }
        }
        reply.ok();
    }

    fn create(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        let parent = u64::from(parent);
        debug!("create(parent={}, name={:?}, mode={:#o})", parent, name, mode);

        if Self::is_dibs_ino(parent) {
            reply.error(Errno::EACCES);
            return;
        }

        let (rel, full) = self.resolve_path(parent, name);
        let c_path = match path_to_cstring(&full) {
            Ok(p) => p,
            Err(_) => {
                reply.error(Errno::EINVAL);
                return;
            }
        };

        // Mark expected write for watcher suppression (both layers)
        self.expected_writes.insert(full.clone());
        self.recent_self_writes.insert(full.clone(), std::time::Instant::now());

        let fd = unsafe { libc::open(c_path.as_ptr(), flags | libc::O_CREAT, mode) };
        if fd < 0 {
            self.expected_writes.remove(&full);
            self.recent_self_writes.remove(&full);
            reply.error(Errno::from(std::io::Error::last_os_error()));
            return;
        }

        let st = match fstat(fd) {
            Ok(st) => st,
            Err(e) => {
                unsafe { libc::close(fd); }
                self.expected_writes.remove(&full);
                reply.error(Errno::from(e));
                return;
            }
        };

        let attr = stat_to_file_attr(&st);
        self.inodes.insert(u64::from(attr.ino), rel.clone());

        let sid = get_sid(req.pid());

        // New file has empty hash
        let hash = vec![];
        self.cas_table.record_read_open(&rel, hash.clone(), sid);
        let fh = self.file_handles.alloc(fd, rel, flags, Some(hash), sid);

        reply.created(&TTL, &attr, Generation(0), FileHandle(fh), FopenFlags::empty());
    }

    fn mkdir(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let parent = u64::from(parent);
        debug!("mkdir(parent={}, name={:?}, mode={:#o})", parent, name, mode);

        if Self::is_dibs_ino(parent) {
            reply.error(Errno::EACCES);
            return;
        }

        let (rel, full) = self.resolve_path(parent, name);
        let c_path = match path_to_cstring(&full) {
            Ok(p) => p,
            Err(_) => {
                reply.error(Errno::EINVAL);
                return;
            }
        };

        self.expected_writes.insert(full.clone());

        let rc = unsafe { libc::mkdir(c_path.as_ptr(), mode as libc::mode_t) };
        if rc != 0 {
            self.expected_writes.remove(&full);
            reply.error(Errno::from(std::io::Error::last_os_error()));
            return;
        }

        match self.lookup_and_register(&rel, &full) {
            Ok(attr) => reply.entry(&TTL, &attr, Generation(0)),
            Err(e) => reply.error(Errno::from(e)),
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let parent = u64::from(parent);
        debug!("unlink(parent={}, name={:?})", parent, name);

        if Self::is_dibs_ino(parent) {
            reply.error(Errno::EACCES);
            return;
        }

        let (rel, full) = self.resolve_path(parent, name);

        // CAS check for unlink — must have a tracked hash that matches
        if let Some(state) = self.cas_table.get(&rel) {
            let state = state.lock();
            if let Some(ref current_hash) = state.hash {
                if let Ok(actual_hash) = cas::hash_file(&full) {
                    if *current_hash != actual_hash {
                        warn!(
                            "CAS conflict on unlink {}: file changed since last read",
                            rel.display()
                        );
                        reply.error(Errno::EIO);
                        return;
                    }
                }
            }
        }

        let c_path = match path_to_cstring(&full) {
            Ok(p) => p,
            Err(_) => {
                reply.error(Errno::EINVAL);
                return;
            }
        };

        self.expected_writes.insert(full.clone());

        let rc = unsafe { libc::unlink(c_path.as_ptr()) };
        if rc != 0 {
            self.expected_writes.remove(&full);
            reply.error(Errno::from(std::io::Error::last_os_error()));
            return;
        }

        self.cas_table.remove(&rel);
        self.inodes.remove_by_path(&rel);
        reply.ok();
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let parent = u64::from(parent);
        debug!("rmdir(parent={}, name={:?})", parent, name);

        if Self::is_dibs_ino(parent) {
            reply.error(Errno::EACCES);
            return;
        }

        let (rel, full) = self.resolve_path(parent, name);
        let c_path = match path_to_cstring(&full) {
            Ok(p) => p,
            Err(_) => {
                reply.error(Errno::EINVAL);
                return;
            }
        };

        self.expected_writes.insert(full.clone());

        let rc = unsafe { libc::rmdir(c_path.as_ptr()) };
        if rc != 0 {
            self.expected_writes.remove(&full);
            reply.error(Errno::from(std::io::Error::last_os_error()));
            return;
        }

        self.inodes.remove_by_path(&rel);
        reply.ok();
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        _flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        let parent = u64::from(parent);
        let newparent = u64::from(newparent);
        debug!(
            "rename(parent={}, name={:?}, newparent={}, newname={:?})",
            parent, name, newparent, newname
        );

        if Self::is_dibs_ino(parent) || Self::is_dibs_ino(newparent) {
            reply.error(Errno::EACCES);
            return;
        }

        let (old_rel, old_full) = self.resolve_path(parent, name);
        let (new_rel, new_full) = self.resolve_path(newparent, newname);

        // CAS check for rename — lock in lexicographic order to prevent deadlocks
        let (_first, _second) = if old_rel <= new_rel {
            (&old_rel, &new_rel)
        } else {
            (&new_rel, &old_rel)
        };

        // Check source CAS
        if let Some(state) = self.cas_table.get(&old_rel) {
            let state = state.lock();
            if let Some(ref current_hash) = state.hash {
                if let Ok(actual_hash) = cas::hash_file(&old_full) {
                    if *current_hash != actual_hash {
                        warn!(
                            "CAS conflict on rename source {}: file changed since last read",
                            old_rel.display()
                        );
                        reply.error(Errno::EIO);
                        return;
                    }
                }
            }
        }

        // Check dest CAS if dest exists and is tracked
        if new_full.exists() {
            if let Some(state) = self.cas_table.get(&new_rel) {
                let state = state.lock();
                if let Some(ref current_hash) = state.hash {
                    if let Ok(actual_hash) = cas::hash_file(&new_full) {
                        if *current_hash != actual_hash {
                            warn!(
                                "CAS conflict on rename dest {}: file changed since last read",
                                new_rel.display()
                            );
                            reply.error(Errno::EIO);
                            return;
                        }
                    }
                }
            }
        }

        let old_c = match path_to_cstring(&old_full) {
            Ok(p) => p,
            Err(_) => {
                reply.error(Errno::EINVAL);
                return;
            }
        };
        let new_c = match path_to_cstring(&new_full) {
            Ok(p) => p,
            Err(_) => {
                reply.error(Errno::EINVAL);
                return;
            }
        };

        self.expected_writes.insert(old_full.clone());
        self.expected_writes.insert(new_full.clone());

        let rc = unsafe { libc::rename(old_c.as_ptr(), new_c.as_ptr()) };
        if rc != 0 {
            self.expected_writes.remove(&old_full);
            self.expected_writes.remove(&new_full);
            reply.error(Errno::from(std::io::Error::last_os_error()));
            return;
        }

        self.inodes.rename(&old_rel, &new_rel);
        self.cas_table.rename(&old_rel, &new_rel);
        reply.ok();
    }

    fn symlink(
        &self,
        _req: &Request,
        parent: INodeNo,
        link_name: &OsStr,
        target: &Path,
        reply: ReplyEntry,
    ) {
        let parent = u64::from(parent);
        debug!("symlink(parent={}, name={:?}, target={:?})", parent, link_name, target);

        if Self::is_dibs_ino(parent) {
            reply.error(Errno::EACCES);
            return;
        }

        let (rel, full) = self.resolve_path(parent, link_name);
        let c_target = match path_to_cstring(target) {
            Ok(p) => p,
            Err(_) => {
                reply.error(Errno::EINVAL);
                return;
            }
        };
        let c_link = match path_to_cstring(&full) {
            Ok(p) => p,
            Err(_) => {
                reply.error(Errno::EINVAL);
                return;
            }
        };

        let rc = unsafe { libc::symlink(c_target.as_ptr(), c_link.as_ptr()) };
        if rc != 0 {
            reply.error(Errno::from(std::io::Error::last_os_error()));
            return;
        }

        match self.lookup_and_register(&rel, &full) {
            Ok(attr) => reply.entry(&TTL, &attr, Generation(0)),
            Err(e) => reply.error(Errno::from(e)),
        }
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        let ino = u64::from(ino);
        debug!("readlink(ino={})", ino);

        let rel = match self.inodes.get_path(ino) {
            Some(p) => p,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        let full = self.backing_path(&rel);

        match std::fs::read_link(&full) {
            Ok(target) => reply.data(target.as_os_str().as_bytes()),
            Err(e) => reply.error(Errno::from(e)),
        }
    }

    fn link(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _newparent: INodeNo,
        _newname: &OsStr,
        reply: ReplyEntry,
    ) {
        // Hard links not supported — they complicate CAS tracking
        reply.error(Errno::ENOTSUP);
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        let c_path = match path_to_cstring(&self.backing) {
            Ok(p) => p,
            Err(_) => {
                reply.error(Errno::EINVAL);
                return;
            }
        };

        unsafe {
            let mut st: libc::statfs = std::mem::zeroed();
            if libc::statfs(c_path.as_ptr(), &mut st) == 0 {
                reply.statfs(
                    st.f_blocks,
                    st.f_bfree,
                    st.f_bavail,
                    st.f_files,
                    st.f_ffree,
                    st.f_bsize as u32,
                    255,
                    st.f_bsize as u32,
                );
            } else {
                reply.error(Errno::from(std::io::Error::last_os_error()));
            }
        }
    }

    fn access(&self, _req: &Request, ino: INodeNo, mask: AccessFlags, reply: ReplyEmpty) {
        let ino = u64::from(ino);
        debug!("access(ino={}, mask={:?})", ino, mask);

        if Self::is_dibs_ino(ino) {
            reply.ok();
            return;
        }

        if ino == 1 {
            reply.ok();
            return;
        }

        let rel = match self.inodes.get_path(ino) {
            Some(p) => p,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        let full = self.backing_path(&rel);
        let c_path = match path_to_cstring(&full) {
            Ok(p) => p,
            Err(_) => {
                reply.error(Errno::EINVAL);
                return;
            }
        };

        let rc = unsafe { libc::access(c_path.as_ptr(), mask.bits()) };
        if rc == 0 {
            reply.ok();
        } else {
            reply.error(Errno::from(std::io::Error::last_os_error()));
        }
    }
}
