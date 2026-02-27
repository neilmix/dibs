use std::fs;

use crate::helpers::TestMount;

/// Test 3: External modification — next write rejected.
/// The CAS check re-hashes the backing file at write time, so external
/// changes are detected immediately without needing a watcher.
#[test]
fn test_external_modification_rejects_write() {
    let mount = TestMount::new();
    let mp = mount.mount_path();

    // Create file through backing
    let backing_file = mount.backing_path().join("external.txt");
    fs::write(&backing_file, "original content").unwrap();

    let mount_file = mp.join("external.txt");

    // Read through mount (establishes reader hash)
    let content = fs::read_to_string(&mount_file).unwrap();
    assert_eq!(content, "original content");

    // Modify directly in backing dir (external change)
    fs::write(&backing_file, "externally modified").unwrap();

    // Try to write through mount — should fail because re-hash at write time
    // detects the backing file no longer matches the reader's hash.
    let result = fs::write(&mount_file, "agent write attempt");
    assert!(
        result.is_err(),
        "Write should fail after external modification"
    );
}
