use std::fs;
use std::process::Command;
use std::time::Duration;

use crate::helpers::{wait_for_file, test_agent_binary, TestMount};

/// Test 1: Two concurrent writers (separate processes/SIDs) — first succeeds, second rejected.
#[test]
fn test_concurrent_writers_second_rejected() {
    let mount = TestMount::new();
    let mp = mount.mount_path();
    let sync_dir = tempfile::tempdir().unwrap();

    // Create a file through the backing dir
    let backing_file = mount.backing_path().join("test.txt");
    fs::write(&backing_file, "initial content").unwrap();

    let mount_file = mp.join("test.txt");
    let agent_bin = test_agent_binary();

    // Spawn Agent A (gets its own SID via setsid)
    let mut agent_a = Command::new(&agent_bin)
        .args([
            mount_file.to_str().unwrap(),
            sync_dir.path().to_str().unwrap(),
            "a",
            "modified by A",
        ])
        .spawn()
        .expect("failed to spawn agent A");

    // Wait for A to read and signal ready
    assert!(
        wait_for_file(&sync_dir.path().join("a.ready"), Duration::from_secs(10)),
        "Agent A did not become ready"
    );

    // Spawn Agent B (gets its own SID via setsid)
    let mut agent_b = Command::new(&agent_bin)
        .args([
            mount_file.to_str().unwrap(),
            sync_dir.path().to_str().unwrap(),
            "b",
            "modified by B",
        ])
        .spawn()
        .expect("failed to spawn agent B");

    // Wait for B to read and signal ready
    assert!(
        wait_for_file(&sync_dir.path().join("b.ready"), Duration::from_secs(10)),
        "Agent B did not become ready"
    );

    // Signal A to write
    fs::write(sync_dir.path().join("a.go"), "").unwrap();

    // Wait for A's result
    assert!(
        wait_for_file(&sync_dir.path().join("a.result"), Duration::from_secs(10)),
        "Agent A did not produce a result"
    );
    let a_result = fs::read_to_string(sync_dir.path().join("a.result")).unwrap();

    // Signal B to write
    fs::write(sync_dir.path().join("b.go"), "").unwrap();

    // Wait for B's result
    assert!(
        wait_for_file(&sync_dir.path().join("b.result"), Duration::from_secs(10)),
        "Agent B did not produce a result"
    );
    let b_result = fs::read_to_string(sync_dir.path().join("b.result")).unwrap();

    let _ = agent_a.wait();
    let _ = agent_b.wait();

    assert_eq!(a_result, "ok", "Agent A write should succeed");
    assert!(
        b_result.starts_with("error"),
        "Agent B write should fail (CAS conflict), got: {}",
        b_result
    );
}

/// Test 2: Read-write-read-write single agent — both succeed.
#[test]
fn test_single_agent_sequential_writes() {
    let mount = TestMount::new();
    let mp = mount.mount_path();

    // Create initial file through the mount
    let mount_file = mp.join("sequential.txt");
    fs::write(&mount_file, "version 1").unwrap();

    // Read
    let content = fs::read_to_string(&mount_file).unwrap();
    assert_eq!(content, "version 1");

    // Write (first time)
    fs::write(&mount_file, "version 2").unwrap();

    // Read again
    let content = fs::read_to_string(&mount_file).unwrap();
    assert_eq!(content, "version 2");

    // Write again (should succeed since we re-read)
    fs::write(&mount_file, "version 3").unwrap();

    // Verify final state
    let content = fs::read_to_string(&mount_file).unwrap();
    assert_eq!(content, "version 3");
}

/// Test 4: Different files — no cross-file conflict.
#[test]
fn test_different_files_no_conflict() {
    let mount = TestMount::new();
    let mp = mount.mount_path();

    // Create two files through the mount
    let mount_a = mp.join("file_a.txt");
    let mount_b = mp.join("file_b.txt");
    fs::write(&mount_a, "content A").unwrap();
    fs::write(&mount_b, "content B").unwrap();

    // Agent A reads file_a
    let _ = fs::read_to_string(&mount_a).unwrap();

    // Agent B reads file_b
    let _ = fs::read_to_string(&mount_b).unwrap();

    // Both write to their respective files — should both succeed
    assert!(fs::write(&mount_a, "new A").is_ok());
    assert!(fs::write(&mount_b, "new B").is_ok());
}

/// Test 5: A reads, B reads, A writes (ok), B writes (rejected) — using separate processes.
#[test]
fn test_ordered_concurrent_access() {
    let mount = TestMount::new();
    let mp = mount.mount_path();
    let sync_dir = tempfile::tempdir().unwrap();

    let backing_file = mount.backing_path().join("ordered.txt");
    fs::write(&backing_file, "original").unwrap();

    let mount_file = mp.join("ordered.txt");
    let agent_bin = test_agent_binary();

    // Spawn A
    let mut agent_a = Command::new(&agent_bin)
        .args([
            mount_file.to_str().unwrap(),
            sync_dir.path().to_str().unwrap(),
            "a",
            "A wrote this",
        ])
        .spawn()
        .expect("failed to spawn agent A");

    assert!(
        wait_for_file(&sync_dir.path().join("a.ready"), Duration::from_secs(10)),
        "Agent A did not become ready"
    );

    // Spawn B
    let mut agent_b = Command::new(&agent_bin)
        .args([
            mount_file.to_str().unwrap(),
            sync_dir.path().to_str().unwrap(),
            "b",
            "B wrote this",
        ])
        .spawn()
        .expect("failed to spawn agent B");

    assert!(
        wait_for_file(&sync_dir.path().join("b.ready"), Duration::from_secs(10)),
        "Agent B did not become ready"
    );

    // A writes first
    fs::write(sync_dir.path().join("a.go"), "").unwrap();
    assert!(
        wait_for_file(&sync_dir.path().join("a.result"), Duration::from_secs(10)),
        "Agent A did not produce a result"
    );
    let a_result = fs::read_to_string(sync_dir.path().join("a.result")).unwrap();

    // B writes second
    fs::write(sync_dir.path().join("b.go"), "").unwrap();
    assert!(
        wait_for_file(&sync_dir.path().join("b.result"), Duration::from_secs(10)),
        "Agent B did not produce a result"
    );
    let b_result = fs::read_to_string(sync_dir.path().join("b.result")).unwrap();

    let _ = agent_a.wait();
    let _ = agent_b.wait();

    assert_eq!(a_result, "ok", "A's write should succeed");
    assert!(
        b_result.starts_with("error"),
        "B's write should fail after A modified the file, got: {}",
        b_result
    );
}

/// Test 6: Two creates with different names — both succeed.
#[test]
fn test_concurrent_creates_different_names() {
    let mount = TestMount::new();
    let mp = mount.mount_path();

    let file_a = mp.join("new_a.txt");
    let file_b = mp.join("new_b.txt");

    // Create both files through mount
    assert!(fs::write(&file_a, "created by A").is_ok());
    assert!(fs::write(&file_b, "created by B").is_ok());

    // Verify both exist
    assert_eq!(fs::read_to_string(&file_a).unwrap(), "created by A");
    assert_eq!(fs::read_to_string(&file_b).unwrap(), "created by B");
}
