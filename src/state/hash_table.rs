use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use parking_lot::Mutex;
use serde::Serialize;
use tracing::debug;

use crate::fs::cas;
use crate::fs::handles::HandleTable;

#[derive(Debug)]
pub struct FileState {
    /// Current known hash of the file.
    pub hash: Option<Vec<u8>>,
    /// File handle that currently owns writes (None if no active writer).
    pub write_owner: Option<u64>,
    /// When this entry was last accessed.
    pub last_access: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
pub struct FileStateInfo {
    pub path: String,
    pub hash: Option<String>,
    pub write_owner: Option<u64>,
    pub last_access: String,
}

pub struct CasTable {
    entries: DashMap<PathBuf, Mutex<FileState>>,
}

impl CasTable {
    pub fn new() -> Self {
        Self {
            entries: DashMap::new(),
        }
    }

    /// Record that a file was opened and its current hash.
    pub fn record_open(&self, path: &Path, hash: Vec<u8>) {
        self.entries
            .entry(path.to_path_buf())
            .and_modify(|state| {
                let mut s = state.lock();
                s.hash = Some(hash.clone());
                s.last_access = Utc::now();
            })
            .or_insert_with(|| {
                Mutex::new(FileState {
                    hash: Some(hash),
                    write_owner: None,
                    last_access: Utc::now(),
                })
            });
    }

    /// Check CAS and acquire write ownership for a handle.
    /// Returns Ok(()) if the write may proceed, Err with description if rejected.
    pub fn check_and_acquire_write(
        &self,
        path: &Path,
        fh: u64,
        handles: &HandleTable,
    ) -> Result<(), String> {
        let entry = match self.entries.get(path) {
            Some(e) => e,
            None => {
                // File not tracked — allow write (new file)
                return Ok(());
            }
        };

        let mut state = entry.lock();

        // If this handle already owns the write, let it through
        if state.write_owner == Some(fh) {
            state.last_access = Utc::now();
            return Ok(());
        }

        // If someone else owns the write, reject
        if let Some(owner) = state.write_owner {
            if owner != fh {
                return Err(format!(
                    "Write ownership conflict on {}: owned by handle {}",
                    path.display(),
                    owner
                ));
            }
        }

        // CAS check: compare handle's hash_at_open with current file hash
        if let Some(handle) = handles.get(fh) {
            if let Some(ref handle_hash) = handle.hash_at_open {
                if let Some(ref current_hash) = state.hash {
                    if handle_hash != current_hash {
                        return Err(format!(
                            "CAS conflict on {}: expected {}, found {}",
                            path.display(),
                            cas::hash_hex(handle_hash),
                            cas::hash_hex(current_hash),
                        ));
                    }
                }
            }
        }

        // Acquire write ownership
        state.write_owner = Some(fh);
        state.last_access = Utc::now();
        debug!("Write ownership acquired on {} by handle {}", path.display(), fh);
        Ok(())
    }

    /// Release write ownership for a handle.
    pub fn release_write(&self, path: &Path, fh: u64) {
        if let Some(entry) = self.entries.get(path) {
            let mut state = entry.lock();
            if state.write_owner == Some(fh) {
                state.write_owner = None;
                debug!("Write ownership released on {} by handle {}", path.display(), fh);
            }
        }
    }

    /// Update the hash for a file (after a successful write + flush).
    pub fn update_hash(&self, path: &Path, hash: Vec<u8>) {
        if let Some(entry) = self.entries.get(path) {
            let mut state = entry.lock();
            state.hash = Some(hash);
            state.last_access = Utc::now();
        }
    }

    /// Invalidate a file's hash (e.g., due to external modification).
    pub fn invalidate(&self, path: &Path) {
        if let Some(entry) = self.entries.get(path) {
            let mut state = entry.lock();
            // Set to a sentinel that will never match any handle's hash_at_open
            state.hash = Some(vec![0xff; 32]);
            debug!("Hash invalidated for {}", path.display());
        }
    }

    /// Check if a file has an active writer.
    pub fn has_active_writer(&self, path: &Path) -> bool {
        self.entries
            .get(path)
            .is_some_and(|entry| entry.lock().write_owner.is_some())
    }

    /// Remove a file from tracking.
    pub fn remove(&self, path: &Path) {
        self.entries.remove(path);
    }

    /// Rename a tracked file.
    pub fn rename(&self, old: &Path, new: &Path) {
        if let Some((_, state)) = self.entries.remove(old) {
            self.entries.insert(new.to_path_buf(), state);
        }
    }

    /// Get the state for a tracked file.
    pub fn get(
        &self,
        path: &Path,
    ) -> Option<dashmap::mapref::one::Ref<'_, PathBuf, Mutex<FileState>>> {
        self.entries.get(path)
    }

    /// Number of tracked files.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Number of active writers.
    pub fn active_writers(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| {
                let s = e.value().lock();
                s.write_owner.is_some()
            })
            .count()
    }

    /// Get all entries for status reporting.
    pub fn all_entries(&self) -> Vec<FileStateInfo> {
        self.entries
            .iter()
            .map(|e| {
                let s = e.value().lock();
                FileStateInfo {
                    path: e.key().display().to_string(),
                    hash: s.hash.as_ref().map(|h| cas::hash_hex(h)),
                    write_owner: s.write_owner,
                    last_access: s.last_access.to_rfc3339(),
                }
            })
            .collect()
    }

    /// Evict entries that haven't been accessed in the given duration.
    pub fn evict_older_than(&self, duration: std::time::Duration) {
        let cutoff = Utc::now() - chrono::Duration::from_std(duration).unwrap_or_default();
        let to_remove: Vec<PathBuf> = self
            .entries
            .iter()
            .filter(|e| {
                let s = e.value().lock();
                s.write_owner.is_none() && s.last_access < cutoff
            })
            .map(|e| e.key().clone())
            .collect();

        for path in to_remove {
            self.entries.remove(&path);
            debug!("Evicted CAS entry for {}", path.display());
        }
    }
}
