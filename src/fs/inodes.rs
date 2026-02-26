use dashmap::DashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Reserved inode range for synthetic .dibs/ entries.
pub const SYNTHETIC_INODE_BASE: u64 = u64::MAX - 1000;

/// Well-known synthetic inodes.
pub const DIBS_DIR_INO: u64 = SYNTHETIC_INODE_BASE;
pub const DIBS_STATUS_INO: u64 = SYNTHETIC_INODE_BASE + 1;
pub const DIBS_LOCKS_INO: u64 = SYNTHETIC_INODE_BASE + 2;
pub const DIBS_CONFLICTS_DIR_INO: u64 = SYNTHETIC_INODE_BASE + 3;

pub struct InodeTable {
    ino_to_path: DashMap<u64, PathBuf>,
    path_to_ino: DashMap<PathBuf, u64>,
    next_synthetic: AtomicU64,
}

impl InodeTable {
    pub fn new() -> Self {
        Self {
            ino_to_path: DashMap::new(),
            path_to_ino: DashMap::new(),
            next_synthetic: AtomicU64::new(DIBS_CONFLICTS_DIR_INO + 1),
        }
    }

    /// Insert or update a mapping using the real inode from stat().
    pub fn insert(&self, ino: u64, path: PathBuf) {
        // Remove any old path mapping for this inode
        if let Some((_, old_path)) = self.ino_to_path.remove(&ino) {
            self.path_to_ino.remove(&old_path);
        }
        // Remove any old inode mapping for this path
        if let Some((_, old_ino)) = self.path_to_ino.remove(&path) {
            if old_ino != ino {
                self.ino_to_path.remove(&old_ino);
            }
        }
        self.ino_to_path.insert(ino, path.clone());
        self.path_to_ino.insert(path, ino);
    }

    pub fn get_path(&self, ino: u64) -> Option<PathBuf> {
        self.ino_to_path.get(&ino).map(|r| r.value().clone())
    }

    pub fn get_ino(&self, path: &Path) -> Option<u64> {
        self.path_to_ino.get(path).map(|r| *r.value())
    }

    pub fn remove_by_ino(&self, ino: u64) {
        if let Some((_, path)) = self.ino_to_path.remove(&ino) {
            self.path_to_ino.remove(&path);
        }
    }

    pub fn remove_by_path(&self, path: &Path) {
        if let Some((_, ino)) = self.path_to_ino.remove(path) {
            self.ino_to_path.remove(&ino);
        }
    }

    /// Rename a path in the inode table.
    pub fn rename(&self, old_path: &Path, new_path: &Path) {
        if let Some((_, ino)) = self.path_to_ino.remove(old_path) {
            self.ino_to_path.insert(ino, new_path.to_path_buf());
            self.path_to_ino.insert(new_path.to_path_buf(), ino);
        }
    }

    /// Allocate a new synthetic inode (for conflict files, etc).
    pub fn alloc_synthetic(&self) -> u64 {
        self.next_synthetic.fetch_add(1, Ordering::Relaxed)
    }

    /// Check if an inode is synthetic.
    pub fn is_synthetic(ino: u64) -> bool {
        ino >= SYNTHETIC_INODE_BASE
    }
}
