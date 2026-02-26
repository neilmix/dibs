use std::sync::Arc;
use std::time::Duration;

use tracing::debug;

use super::hash_table::CasTable;

/// Start a background thread that periodically evicts stale CAS entries.
///
/// Safety: The caller must ensure that `cas_table` remains valid for the lifetime
/// of the returned thread. In practice, the DibsFs owns both the CasTable and
/// signals the shutdown flag, so the table outlives the thread.
pub fn start_eviction_thread(
    cas_table: *const CasTable,
    eviction_minutes: u64,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
) -> std::thread::JoinHandle<()> {
    let check_interval = Duration::from_secs(60);
    let eviction_duration = Duration::from_secs(eviction_minutes * 60);
    // Convert to usize to cross the thread boundary — usize is Send.
    let ptr_val = cas_table as usize;

    std::thread::Builder::new()
        .name("dibs-eviction".to_string())
        .spawn(move || {
            debug!("Eviction thread started, eviction_minutes={}", eviction_minutes);
            while !shutdown.load(std::sync::atomic::Ordering::Relaxed) {
                // Sleep in 1-second ticks so we notice the shutdown flag promptly.
                // A single sleep(check_interval) blocked shutdown for up to 60s,
                // causing the ctrl-C hang bug — eviction_handle.join() in main
                // would wait the full interval before the thread could exit.
                let mut remaining = check_interval;
                let tick = Duration::from_secs(1);
                while remaining > Duration::ZERO {
                    if shutdown.load(std::sync::atomic::Ordering::Relaxed) {
                        break;
                    }
                    let sleep_time = remaining.min(tick);
                    std::thread::sleep(sleep_time);
                    remaining = remaining.saturating_sub(sleep_time);
                }
                if shutdown.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }
                let table = unsafe { &*(ptr_val as *const CasTable) };
                table.evict_older_than(eviction_duration);
            }
            debug!("Eviction thread shutting down");
        })
        .expect("failed to spawn eviction thread")
}
