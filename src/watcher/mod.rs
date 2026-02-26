use std::path::PathBuf;

use notify::{Event, EventKind, RecursiveMode, Watcher};
use tracing::{debug, error};

use crate::fs::DibsFs;

/// Start the filesystem watcher on the backing directory.
/// This detects external modifications and invalidates CAS entries.
pub fn start_watcher(fs: &mut DibsFs) {
    let backing = fs.backing.clone();

    // We need raw pointers to access the DibsFs fields from the watcher callback,
    // because notify requires 'static. This is safe because the watcher is dropped
    // in DibsFs::destroy() before the DibsFs itself is dropped.
    let cas_table_ptr = &fs.cas_table as *const _ as usize;
    let expected_writes_ptr = &fs.expected_writes as *const _ as usize;
    let backing_clone = backing.clone();

    let watcher_result = notify::recommended_watcher(move |res: Result<Event, notify::Error>| {
        let cas_table =
            unsafe { &*(cas_table_ptr as *const crate::state::hash_table::CasTable) };
        let expected_writes =
            unsafe { &*(expected_writes_ptr as *const dashmap::DashSet<PathBuf>) };

        match res {
            Ok(event) => {
                handle_event(event, cas_table, expected_writes, &backing_clone);
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
    cas_table: &crate::state::hash_table::CasTable,
    expected_writes: &dashmap::DashSet<PathBuf>,
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
