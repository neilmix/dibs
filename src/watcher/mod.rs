use std::path::PathBuf;
use std::sync::Arc;

use dashmap::DashSet;
use notify::{Event, EventKind, RecursiveMode, Watcher};
use tracing::{debug, error};

use crate::fs::DibsFs;
use crate::state::hash_table::CasTable;

/// Start the filesystem watcher on the backing directory.
/// This detects external modifications and invalidates CAS entries.
pub fn start_watcher(fs: &mut DibsFs) {
    let backing = fs.backing.clone();

    let cas_table = Arc::clone(&fs.cas_table);
    let expected_writes = Arc::clone(&fs.expected_writes);
    let backing_clone = backing.clone();

    let watcher_result = notify::recommended_watcher(move |res: Result<Event, notify::Error>| {
        match res {
            Ok(event) => {
                handle_event(event, &cas_table, &expected_writes, &backing_clone);
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
    backing: &PathBuf,
) {
    match event.kind {
        EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_) => {
            for path in &event.paths {
                // Self-write suppression
                if expected_writes.remove(path).is_some() {
                    debug!("Suppressed self-write event for {}", path.display());
                    continue;
                }

                // Convert to relative path
                if let Ok(rel) = path.strip_prefix(backing) {
                    let rel_buf = rel.to_path_buf();
                    debug!("External modification detected: {}", rel_buf.display());
                    cas_table.invalidate(&rel_buf);
                }
            }
        }
        _ => {}
    }
}
