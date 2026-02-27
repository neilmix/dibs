use std::env;
use std::fs;
use std::path::Path;
use std::thread;
use std::time::Duration;

/// A small test agent binary that reads a file, signals readiness, waits for
/// a go signal, then writes. Used by integration tests to simulate separate
/// agents with different session IDs (SIDs).
///
/// Usage: dibs-test-agent <file_path> <sync_dir> <agent_name> <write_content>
fn main() {
    // Create a new session so this process gets its own SID,
    // distinct from the test runner and other agents.
    unsafe {
        libc::setsid();
    }

    let args: Vec<String> = env::args().collect();
    if args.len() != 5 {
        eprintln!(
            "Usage: {} <file_path> <sync_dir> <agent_name> <write_content>",
            args[0]
        );
        std::process::exit(1);
    }

    let file_path = &args[1];
    let sync_dir = Path::new(&args[2]);
    let agent_name = &args[3];
    let content = &args[4];

    // Read the file (goes through FUSE, records our SID)
    match fs::read_to_string(file_path) {
        Ok(_data) => {}
        Err(e) => {
            let _ = fs::write(
                sync_dir.join(format!("{}.result", agent_name)),
                format!("read_error: {}", e),
            );
            // Still signal ready so the test doesn't hang
            let _ = fs::write(sync_dir.join(format!("{}.ready", agent_name)), "");
            return;
        }
    }

    // Signal ready
    fs::write(sync_dir.join(format!("{}.ready", agent_name)), "").unwrap();

    // Wait for go signal
    let go_path = sync_dir.join(format!("{}.go", agent_name));
    for _ in 0..200 {
        // 10 second timeout
        if go_path.exists() {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    if !go_path.exists() {
        let _ = fs::write(
            sync_dir.join(format!("{}.result", agent_name)),
            "timeout waiting for go signal",
        );
        return;
    }

    // Write to the file (goes through FUSE, triggers CAS check)
    match fs::write(file_path, content) {
        Ok(_) => {
            fs::write(
                sync_dir.join(format!("{}.result", agent_name)),
                "ok",
            )
            .unwrap();
        }
        Err(e) => {
            fs::write(
                sync_dir.join(format!("{}.result", agent_name)),
                format!("error: {}", e),
            )
            .unwrap();
        }
    }
}
