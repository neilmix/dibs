use std::fs;
use std::time::Duration;

use crate::helpers::TestMount;

/// Test 7: Delete of file being edited — rejected.
#[test]
fn test_delete_tracked_file_with_external_change() {
    let mount = TestMount::new();
    let mp = mount.mount_path();

    // Create file
    let backing_file = mount.backing_path().join("deleteme.txt");
    fs::write(&backing_file, "content to delete").unwrap();

    let mount_file = mp.join("deleteme.txt");

    // Read through mount (track it)
    let _ = fs::read_to_string(&mount_file).unwrap();

    // Modify externally
    fs::write(&backing_file, "externally changed").unwrap();
    std::thread::sleep(Duration::from_secs(2));

    // Try to delete through mount — should fail (hash mismatch)
    let result = fs::remove_file(&mount_file);
    assert!(
        result.is_err(),
        "Delete should fail when file was externally modified"
    );
}
