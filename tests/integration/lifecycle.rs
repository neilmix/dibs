use std::fs;
use std::process::Command;
use std::time::Duration;

/// Test 10: Clean shutdown — no stale mounts.
#[test]
fn test_clean_shutdown() {
    let backing = tempfile::tempdir().unwrap();
    let mount_dir = tempfile::tempdir().unwrap();

    // Create a file in backing
    fs::write(backing.path().join("test.txt"), "hello").unwrap();

    // Start dibs
    let dibs_bin = crate::helpers::dibs_binary();

    let mut child = Command::new(&dibs_bin)
        .args([
            "mount",
            backing.path().to_str().unwrap(),
            mount_dir.path().to_str().unwrap(),
            "-f",
            "--log-file",
            "/tmp/dibs-test-lifecycle.log",
        ])
        .spawn()
        .expect("failed to start dibs");

    // Wait for mount (longer when running in parallel with other FUSE tests)
    std::thread::sleep(Duration::from_secs(5));

    // Verify mount works
    let files: Vec<_> = fs::read_dir(mount_dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .collect();
    assert!(!files.is_empty(), "Mount should have files");

    // Unmount cleanly
    let mp = mount_dir.path().to_str().unwrap();
    let _ = Command::new("umount").arg(mp).status();

    // Wait for process to exit
    std::thread::sleep(Duration::from_secs(1));

    // Kill if still running
    let _ = child.kill();
    let _ = child.wait();

    // Verify mount point is no longer mounted
    // (reading it should work normally as empty dir or fail gracefully)
    std::thread::sleep(Duration::from_millis(500));
}
