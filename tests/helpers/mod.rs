use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::Duration;

/// A test mount that manages backing dir, mount point, and the dibs process.
pub struct TestMount {
    pub backing: tempfile::TempDir,
    pub mount_dir: tempfile::TempDir,
    pub process: Option<Child>,
}

impl TestMount {
    /// Create a new test mount. Starts dibs in the background.
    pub fn new() -> Self {
        let backing = tempfile::tempdir().expect("failed to create backing dir");
        let mount_dir = tempfile::tempdir().expect("failed to create mount dir");

        let dibs_bin = dibs_binary();
        let child = Command::new(&dibs_bin)
            .args([
                "mount",
                backing.path().to_str().unwrap(),
                mount_dir.path().to_str().unwrap(),
                "-f",
                "--log-file",
                "/tmp/dibs-test.log",
                "--eviction-minutes",
                "60",
                "--save-conflicts",
            ])
            .spawn()
            .expect("failed to start dibs");

        // Wait for mount to be ready
        wait_for_mount(mount_dir.path());

        TestMount {
            backing,
            mount_dir,
            process: Some(child),
        }
    }

    pub fn backing_path(&self) -> &Path {
        self.backing.path()
    }

    pub fn mount_path(&self) -> &Path {
        self.mount_dir.path()
    }
}

impl Drop for TestMount {
    fn drop(&mut self) {
        if let Some(mut child) = self.process.take() {
            // Unmount
            let mp = self.mount_dir.path().to_str().unwrap();
            let _ = Command::new("umount").arg(mp).status();
            // Give it a moment
            std::thread::sleep(Duration::from_millis(500));
            // Kill if still running
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

pub fn dibs_binary() -> PathBuf {
    // Look for the binary in the target directory
    let mut path = std::env::current_exe()
        .expect("failed to get test binary path");
    // Go up from the test binary to the target dir
    path.pop(); // remove binary name
    path.pop(); // remove deps/
    path.push("dibs");
    if path.exists() {
        return path;
    }
    // Try the x86_64 target directory
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("target");
    path.push("x86_64-apple-darwin");
    path.push("debug");
    path.push("dibs");
    if path.exists() {
        return path;
    }
    // Fallback
    PathBuf::from("target/debug/dibs")
}

fn wait_for_mount(mount_path: &Path) {
    for _ in 0..50 {
        // Check if the mount point has the FUSE filesystem
        if let Ok(entries) = std::fs::read_dir(mount_path) {
            // If we can read the directory and it appears mounted, we're good
            // A mounted FUSE filesystem will have .dibs virtual directory
            for entry in entries {
                if let Ok(entry) = entry {
                    if entry.file_name() == ".dibs" {
                        return;
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    // Even if .dibs doesn't show up, the mount might still be ready
    // Give it one more second
    std::thread::sleep(Duration::from_secs(1));
}
