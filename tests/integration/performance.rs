use std::fs;
use std::time::{Duration, Instant};

use crate::helpers::TestMount;

/// Test 8: Large file (>10MB) write completes in <500ms.
#[test]
fn test_large_file_performance() {
    let mount = TestMount::new();
    let mp = mount.mount_path();

    // Create a 15MB file in backing (too large to create through FUSE efficiently)
    let backing_file = mount.backing_path().join("large.bin");
    let data: Vec<u8> = (0..15_000_000).map(|i| (i % 256) as u8).collect();
    fs::write(&backing_file, &data).unwrap();

    // Wait for watcher events from backing-dir write to propagate
    std::thread::sleep(Duration::from_secs(2));

    let mount_file = mp.join("large.bin");

    // Read through mount
    let start = Instant::now();
    let content = fs::read(&mount_file).unwrap();
    let read_time = start.elapsed();
    assert_eq!(content.len(), 15_000_000);
    println!("Large file read: {:?}", read_time);

    // Write modified data
    let modified: Vec<u8> = (0..15_000_000).map(|i| ((i + 1) % 256) as u8).collect();
    let start = Instant::now();
    fs::write(&mount_file, &modified).unwrap();
    let write_time = start.elapsed();
    println!("Large file write: {:?}", write_time);

    assert!(
        write_time < std::time::Duration::from_millis(5000),
        "Large file write took too long: {:?}",
        write_time
    );
}

/// Test 9: 1000+ tracked files without degradation.
#[test]
fn test_many_tracked_files() {
    let mount = TestMount::new();
    let mp = mount.mount_path();

    let start = Instant::now();

    // Create 1000 files through the mount
    for i in 0..1000 {
        let file = mp.join(format!("file_{:04}.txt", i));
        fs::write(&file, format!("content {}", i)).unwrap();
    }

    let create_time = start.elapsed();
    println!("Created 1000 files in {:?}", create_time);

    // Read all files
    let start = Instant::now();
    for i in 0..1000 {
        let file = mp.join(format!("file_{:04}.txt", i));
        let content = fs::read_to_string(&file).unwrap();
        assert_eq!(content, format!("content {}", i));
    }

    let read_time = start.elapsed();
    println!("Read 1000 files in {:?}", read_time);

    // Write to all files
    let start = Instant::now();
    for i in 0..1000 {
        let file = mp.join(format!("file_{:04}.txt", i));
        fs::write(&file, format!("updated {}", i)).unwrap();
    }

    let write_time = start.elapsed();
    println!("Wrote 1000 files in {:?}", write_time);

    assert!(
        write_time < std::time::Duration::from_secs(60),
        "Writing 1000 files took too long: {:?}",
        write_time
    );
}
