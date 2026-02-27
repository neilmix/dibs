use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::{DashMap, DashSet};
use notify::{Event, EventKind, RecursiveMode, Watcher};
use tracing::{debug, error};

use crate::fs::DibsFs;
use crate::state::hash_table::CasTable;

/// How long to suppress watcher events after a self-write flush.
const RECENT_WRITE_TTL: Duration = Duration::from_secs(2);

/// Start the filesystem watcher on the backing directory.
/// This detects external modifications and invalidates CAS entries.
pub fn start_watcher(fs: &mut DibsFs) {
    let backing = fs.backing.clone();

    let cas_table = Arc::clone(&fs.cas_table);
    let expected_writes = Arc::clone(&fs.expected_writes);
    let recent_self_writes = Arc::clone(&fs.recent_self_writes);
    let backing_clone = backing.clone();

    let watcher_result = notify::recommended_watcher(move |res: Result<Event, notify::Error>| {
        match res {
            Ok(event) => {
                handle_event(event, &cas_table, &expected_writes, &recent_self_writes, &backing_clone);
            }
            Err(e) => {
                error!("Watcher error: {}", e);
            }
        }
    });

    match watcher_result {
        Ok(mut watcher) => {
            if let Err(e) = watcher.watch(&backing, RecursiveMode::Recursive) {
                error!("Failed to watch backing directory: {}", e);
                return;
            }
            debug!("File watcher started on {}", backing.display());
            *fs.watcher.lock() = Some(watcher);
        }
        Err(e) => {
            error!("Failed to create file watcher: {}", e);
        }
    }
}

fn handle_event(
    event: Event,
    cas_table: &CasTable,
    expected_writes: &DashSet<PathBuf>,
    recent_self_writes: &DashMap<PathBuf, Instant>,
    backing: &PathBuf,
) {
    match event.kind {
        EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_) => {
            for path in &event.paths {
                // Layer 1: self-write suppression via expected_writes
                if expected_writes.remove(path).is_some() {
                    debug!("Suppressed self-write event for {}", path.display());
                    continue;
                }

                // Convert to relative path
                if let Ok(rel) = path.strip_prefix(backing) {
                    let rel_buf = rel.to_path_buf();
                    // Layer 2: a single FUSE op (e.g. create with
                    // O_CREAT|O_TRUNC) can emit multiple FS events, but only one
                    // expected_writes entry exists. The active-writer check catches
                    // the extra events that slip past expected_writes.remove().
                    if cas_table.has_active_writer(&rel_buf) {
                        debug!(
                            "Skipping invalidation for {} (active writer)",
                            rel_buf.display()
                        );
                        continue;
                    }

                    // Layer 3: recently flushed self-writes. After flush releases
                    // the write owner and removes expected_writes, delayed watcher
                    // events (macOS FSEvents latency) can still arrive. Suppress
                    // events within RECENT_WRITE_TTL of the last flush.
                    if let Some(entry) = recent_self_writes.get(path) {
                        if entry.value().elapsed() < RECENT_WRITE_TTL {
                            debug!(
                                "Suppressed delayed self-write event for {} (flushed {:?} ago)",
                                rel_buf.display(),
                                entry.value().elapsed()
                            );
                            continue;
                        }
                        // Expired — clean up and proceed to invalidate
                        drop(entry);
                        recent_self_writes.remove(path);
                    }

                    debug!("External modification detected: {}", rel_buf.display());
                    cas_table.invalidate(&rel_buf);
                }
            }
        }
        _ => {}
    }
}
