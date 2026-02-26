use std::fs;

use crate::helpers::TestMount;

/// Test 1: Two concurrent writers — first succeeds, second rejected.
#[test]
fn test_concurrent_writers_second_rejected() {
    let mount = TestMount::new();
    let mp = mount.mount_path();

    // Create a file through the backing dir
    let backing_file = mount.backing_path().join("test.txt");
    fs::write(&backing_file, "initial content").unwrap();

    let mount_file = mp.join("test.txt");

    // Agent A opens and reads
    let content_a = fs::read_to_string(&mount_file).unwrap();
    assert_eq!(content_a, "initial content");

    // Agent B opens and reads
    let content_b = fs::read_to_string(&mount_file).unwrap();
    assert_eq!(content_b, "initial content");

    // Agent A writes — should succeed
    let result_a = fs::write(&mount_file, "modified by A");
    assert!(result_a.is_ok(), "Agent A write should succeed");

    // Agent B writes — should fail with EIO (hash mismatch)
    let result_b = fs::write(&mount_file, "modified by B");
    assert!(result_b.is_err(), "Agent B write should fail (CAS conflict)");
}

/// Test 2: Read-write-read-write single agent — both succeed.
#[test]
fn test_single_agent_sequential_writes() {
    let mount = TestMount::new();
    let mp = mount.mount_path();

    // Create initial file
    let backing_file = mount.backing_path().join("sequential.txt");
    fs::write(&backing_file, "version 1").unwrap();

    let mount_file = mp.join("sequential.txt");

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

    // Create two files
    let backing_a = mount.backing_path().join("file_a.txt");
    let backing_b = mount.backing_path().join("file_b.txt");
    fs::write(&backing_a, "content A").unwrap();
    fs::write(&backing_b, "content B").unwrap();

    let mount_a = mp.join("file_a.txt");
    let mount_b = mp.join("file_b.txt");

    // Agent A reads file_a
    let _ = fs::read_to_string(&mount_a).unwrap();

    // Agent B reads file_b
    let _ = fs::read_to_string(&mount_b).unwrap();

    // Both write to their respective files — should both succeed
    assert!(fs::write(&mount_a, "new A").is_ok());
    assert!(fs::write(&mount_b, "new B").is_ok());
}

/// Test 5: A reads, B reads, A writes (ok), B writes (rejected).
#[test]
fn test_ordered_concurrent_access() {
    let mount = TestMount::new();
    let mp = mount.mount_path();

    let backing_file = mount.backing_path().join("ordered.txt");
    fs::write(&backing_file, "original").unwrap();

    let mount_file = mp.join("ordered.txt");

    // A reads
    let _ = fs::read_to_string(&mount_file).unwrap();

    // B reads
    let _ = fs::read_to_string(&mount_file).unwrap();

    // A writes — succeeds
    assert!(fs::write(&mount_file, "A wrote this").is_ok());

    // B writes — should fail (A changed the file)
    let result = fs::write(&mount_file, "B wrote this");
    assert!(result.is_err(), "B's write should fail after A modified the file");
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
