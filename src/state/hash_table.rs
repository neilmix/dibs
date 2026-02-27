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
    /// File handle that currently owns writes (None if no active writer).
    pub write_owner: Option<u64>,
    /// When this entry was last accessed.
    pub last_access: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct ReaderEntry {
    pub hash: Vec<u8>,
    pub last_access: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
pub struct FileStateInfo {
    pub path: String,
    pub write_owner: Option<u64>,
    pub last_access: String,
}

pub struct CasTable {
    entries: DashMap<PathBuf, Mutex<FileState>>,
    reader_hashes: DashMap<(u32, PathBuf), ReaderEntry>,
}

impl CasTable {
    pub fn new() -> Self {
        Self {
            entries: DashMap::new(),
            reader_hashes: DashMap::new(),
        }
    }

    /// Record a reader's hash for a (SID, path) pair.
    /// Called when a file is opened for reading (O_RDONLY or O_RDWR).
    pub fn record_reader(&self, path: &Path, hash: Vec<u8>, sid: u32) {
        self.reader_hashes.insert(
            (sid, path.to_path_buf()),
            ReaderEntry {
                hash,
                last_access: Utc::now(),
            },
        );
    }

    /// Ensure a write-ownership entry exists for a path.
    /// Does NOT record any hash — only needed so write_owner can be tracked.
    pub fn ensure_entry(&self, path: &Path) {
        self.entries
            .entry(path.to_path_buf())
            .or_insert_with(|| {
                Mutex::new(FileState {
                    write_owner: None,
                    last_access: Utc::now(),
                })
            });
    }

    /// Check CAS and acquire write ownership for a handle.
    ///
    /// `actual_hash` is the current hash of the backing file, computed by the caller.
    /// The CAS check compares this against the reader's hash (what the session last saw).
    ///
    /// Returns Ok(()) if the write may proceed, Err with description if rejected.
    pub fn check_and_acquire_write(
        &self,
        path: &Path,
        fh: u64,
        sid: u32,
        handles: &HandleTable,
        actual_hash: &[u8],
    ) -> Result<(), String> {
        // Ensure entry exists for write_owner tracking
        self.ensure_entry(path);

        let entry = self.entries.get(path).unwrap();
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

        // CAS check: compare reader's hash against actual file hash
        if let Some(handle) = handles.get(fh) {
            if let Some(ref handle_hash) = handle.hash_at_open {
                // O_RDWR case: compare handle's hash_at_open with actual hash
                if handle_hash != actual_hash {
                    return Err(format!(
                        "CAS conflict on {}: expected {}, found {}",
                        path.display(),
                        cas::hash_hex(handle_hash),
                        cas::hash_hex(actual_hash),
                    ));
                }
            } else {
                // O_WRONLY case: look up reader_hashes for this SID
                if let Some(reader) = self.reader_hashes.get(&(sid, path.to_path_buf())) {
                    if reader.hash != actual_hash {
                        return Err(format!(
                            "CAS conflict on {}: reader hash {}, current {}",
                            path.display(),
                            cas::hash_hex(&reader.hash),
                            cas::hash_hex(actual_hash),
                        ));
                    }
                }
                // If no reader entry: blind write — no prior read to conflict with
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

    /// Update the reader hash for a SID after a successful write + flush.
    pub fn update_reader(&self, sid: u32, path: &Path, hash: Vec<u8>) {
        self.reader_hashes.insert(
            (sid, path.to_path_buf()),
            ReaderEntry {
                hash,
                last_access: Utc::now(),
            },
        );
    }

    /// Get the reader hash for a (SID, path) pair, if it exists.
    pub fn get_reader_hash(&self, sid: u32, path: &Path) -> Option<Vec<u8>> {
        self.reader_hashes
            .get(&(sid, path.to_path_buf()))
            .map(|entry| entry.hash.clone())
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
        self.reader_hashes.retain(|k, _| k.1 != *path);
    }

    /// Rename a tracked file.
    pub fn rename(&self, old: &Path, new: &Path) {
        if let Some((_, state)) = self.entries.remove(old) {
            self.entries.insert(new.to_path_buf(), state);
        }
        let to_move: Vec<(u32, ReaderEntry)> = self
            .reader_hashes
            .iter()
            .filter(|e| e.key().1 == *old)
            .map(|e| (e.key().0, e.value().clone()))
            .collect();
        for (sid, entry) in to_move {
            self.reader_hashes.remove(&(sid, old.to_path_buf()));
            self.reader_hashes.insert((sid, new.to_path_buf()), entry);
        }
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

        // Also evict stale reader entries
        self.reader_hashes.retain(|_, v| v.last_access >= cutoff);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::handles::HandleTable;
    use std::path::PathBuf;

    fn make_hash(byte: u8) -> Vec<u8> {
        vec![byte; 32]
    }

    /// Two different SIDs: read, read, write (ok), write (conflict)
    #[test]
    fn test_two_sids_conflict() {
        let cas = CasTable::new();
        let handles = HandleTable::new();
        let path = PathBuf::from("test.txt");
        let h0 = make_hash(0xAA);

        // SID 100 reads
        cas.record_reader(&path, h0.clone(), 100);
        // SID 200 reads
        cas.record_reader(&path, h0.clone(), 200);

        // SID 100 opens for write (O_WRONLY → hash_at_open = None)
        let fh1 = handles.alloc(-1, path.clone(), libc::O_WRONLY, None, 100);
        cas.ensure_entry(&path);

        // SID 100 writes — actual hash matches reader hash, should succeed
        let result = cas.check_and_acquire_write(&path, fh1, 100, &handles, &h0);
        assert!(result.is_ok(), "SID 100 write should succeed");

        // Simulate flush: update reader hash
        let h_a = make_hash(0xBB);
        cas.update_reader(100, &path, h_a.clone());
        cas.release_write(&path, fh1);

        // SID 200 opens for write (O_WRONLY → hash_at_open = None)
        let fh2 = handles.alloc(-1, path.clone(), libc::O_WRONLY, None, 200);

        // SID 200 writes — actual hash is now h_a (0xBB), reader hash is h0 (0xAA)
        let result = cas.check_and_acquire_write(&path, fh2, 200, &handles, &h_a);
        assert!(result.is_err(), "SID 200 write should fail with CAS conflict");
    }

    /// Blind write (no reader entry): allowed
    #[test]
    fn test_blind_write_allowed() {
        let cas = CasTable::new();
        let handles = HandleTable::new();
        let path = PathBuf::from("test.txt");

        // File exists in reader_hashes for SID 100, but not SID 300
        cas.record_reader(&path, make_hash(0xAA), 100);

        // SID 300 opens for write without reading first
        let fh = handles.alloc(-1, path.clone(), libc::O_WRONLY, None, 300);

        // Should succeed — no reader entry for SID 300, so it's a blind write
        let actual = make_hash(0xBB); // file could be anything
        let result = cas.check_and_acquire_write(&path, fh, 300, &handles, &actual);
        assert!(result.is_ok(), "Blind write should be allowed");
    }

    /// Same SID sequential read-write-read-write: all succeed
    #[test]
    fn test_same_sid_sequential() {
        let cas = CasTable::new();
        let handles = HandleTable::new();
        let path = PathBuf::from("test.txt");
        let h0 = make_hash(0xAA);

        // Read
        cas.record_reader(&path, h0.clone(), 100);

        // Write (O_WRONLY) — actual hash matches reader hash
        let fh1 = handles.alloc(-1, path.clone(), libc::O_WRONLY, None, 100);
        let result = cas.check_and_acquire_write(&path, fh1, 100, &handles, &h0);
        assert!(result.is_ok(), "First write should succeed");

        // Flush
        let h1 = make_hash(0xBB);
        cas.update_reader(100, &path, h1.clone());
        cas.release_write(&path, fh1);

        // Read again (update reader hash)
        cas.record_reader(&path, h1.clone(), 100);

        // Write again (O_WRONLY) — actual hash matches new reader hash
        let fh2 = handles.alloc(-1, path.clone(), libc::O_WRONLY, None, 100);
        let result = cas.check_and_acquire_write(&path, fh2, 100, &handles, &h1);
        assert!(result.is_ok(), "Second write should succeed");
    }

    /// Eviction of stale reader entries
    #[test]
    fn test_eviction_cleans_reader_hashes() {
        let cas = CasTable::new();
        let path = PathBuf::from("test.txt");
        let h0 = make_hash(0xAA);

        cas.record_reader(&path, h0.clone(), 100);
        cas.record_reader(&path, h0.clone(), 200);
        // Also create an entry so eviction has something to clean
        cas.ensure_entry(&path);

        assert!(cas.reader_hashes.contains_key(&(100, path.clone())));
        assert!(cas.reader_hashes.contains_key(&(200, path.clone())));

        // Eviction with zero duration removes everything
        cas.evict_older_than(std::time::Duration::from_secs(0));

        assert!(cas.reader_hashes.is_empty(), "Reader hashes should be evicted");
        assert_eq!(cas.entries.len(), 0, "CAS entries should be evicted");
    }

    /// Remove cleans up reader_hashes
    #[test]
    fn test_remove_cleans_reader_hashes() {
        let cas = CasTable::new();
        let path = PathBuf::from("test.txt");

        cas.record_reader(&path, make_hash(0xAA), 100);
        cas.record_reader(&path, make_hash(0xAA), 200);
        assert_eq!(cas.reader_hashes.len(), 2);

        cas.remove(&path);
        assert_eq!(cas.reader_hashes.len(), 0);
    }

    /// Rename moves reader_hashes
    #[test]
    fn test_rename_moves_reader_hashes() {
        let cas = CasTable::new();
        let old = PathBuf::from("old.txt");
        let new = PathBuf::from("new.txt");

        cas.record_reader(&old, make_hash(0xAA), 100);
        cas.record_reader(&old, make_hash(0xAA), 200);
        cas.ensure_entry(&old);

        cas.rename(&old, &new);

        assert!(!cas.reader_hashes.contains_key(&(100, old.clone())));
        assert!(!cas.reader_hashes.contains_key(&(200, old.clone())));
        assert!(cas.reader_hashes.contains_key(&(100, new.clone())));
        assert!(cas.reader_hashes.contains_key(&(200, new.clone())));
        assert!(cas.entries.contains_key(&new));
        assert!(!cas.entries.contains_key(&old));
    }

    /// O_RDWR handle uses hash_at_open for CAS check
    #[test]
    fn test_rdwr_uses_hash_at_open() {
        let cas = CasTable::new();
        let handles = HandleTable::new();
        let path = PathBuf::from("test.txt");
        let h0 = make_hash(0xAA);

        cas.record_reader(&path, h0.clone(), 100);

        // O_RDWR handle has hash_at_open set
        let fh = handles.alloc(-1, path.clone(), libc::O_RDWR, Some(h0.clone()), 100);

        // Actual file hash has changed (another agent wrote)
        let h1 = make_hash(0xBB);

        // Write should fail — hash_at_open (0xAA) != actual hash (0xBB)
        let result = cas.check_and_acquire_write(&path, fh, 100, &handles, &h1);
        assert!(result.is_err(), "O_RDWR write should fail when file hash changed");
    }
}
