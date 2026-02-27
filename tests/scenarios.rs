//! Shutdown scenario tests for dibs.
//!
//! These tests require a working FUSE installation (macFUSE, FUSE-T, or libfuse).
//! They are ignored by default. Run with:
//!
//!     cargo test --test scenarios -- --ignored --test-threads=1

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// A running `dibs mount` process with helpers for inspecting and controlling it.
struct DibsMount {
    child: Child,
    _backing: TempDir,
    mountpoint: TempDir,
    stderr_lines: Arc<Mutex<Vec<String>>>,
    _stderr_thread: Option<std::thread::JoinHandle<()>>,
}

impl DibsMount {
    /// Spawn `dibs mount <backing> <mountpoint>` with fresh temp directories.
    fn start() -> Self {
        let backing = tempfile::tempdir().expect("create backing tmpdir");
        let mountpoint = tempfile::tempdir().expect("create mountpoint tmpdir");

        // Write a seed file into the backing dir so the mount has something to
        // serve — useful for tests that need to open files inside the mount.
        std::fs::write(backing.path().join("hello.txt"), "hello\n")
            .expect("write seed file");

        let log_file = backing.path().join("dibs-test.log");

        let mut child = Command::new(env!("CARGO_BIN_EXE_dibs"))
            .args([
                "mount",
                backing.path().to_str().unwrap(),
                mountpoint.path().to_str().unwrap(),
                "--log-file",
                log_file.to_str().unwrap(),
            ])
            .stderr(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .expect("failed to spawn dibs mount");

        let stderr = child.stderr.take().unwrap();
        let lines: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let lines_clone = lines.clone();

        let thread = std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().flatten() {
                lines_clone.lock().unwrap().push(line);
            }
        });

        Self {
            child,
            _backing: backing,
            mountpoint,
            stderr_lines: lines,
            _stderr_thread: Some(thread),
        }
    }

    fn mountpoint(&self) -> &Path {
        self.mountpoint.path()
    }

    fn pid(&self) -> libc::pid_t {
        self.child.id() as libc::pid_t
    }

    /// Block until the mountpoint appears in `mount` output, or panic on timeout.
    fn wait_for_mount(&self, timeout: Duration) {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if is_mounted(self.mountpoint()) {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let stderr = self.stderr_snapshot();
        panic!(
            "dibs mount did not appear at {:?} within {:?}\nstderr so far:\n{}",
            self.mountpoint(),
            timeout,
            stderr.join("\n"),
        );
    }

    fn send_signal(&self, sig: libc::c_int) {
        unsafe {
            libc::kill(self.pid(), sig);
        }
    }

    /// Poll `try_wait` until the process exits or timeout elapses.
    fn wait_with_timeout(&mut self, timeout: Duration) -> Option<ExitStatus> {
        let start = Instant::now();
        loop {
            match self.child.try_wait().expect("try_wait failed") {
                Some(status) => return Some(status),
                None if start.elapsed() >= timeout => return None,
                None => std::thread::sleep(Duration::from_millis(50)),
            }
        }
    }

    fn is_running(&mut self) -> bool {
        self.child.try_wait().expect("try_wait failed").is_none()
    }

    fn stderr_snapshot(&self) -> Vec<String> {
        self.stderr_lines.lock().unwrap().clone()
    }

    fn stderr_contains(&self, pattern: &str) -> bool {
        self.stderr_lines
            .lock()
            .unwrap()
            .iter()
            .any(|line| line.contains(pattern))
    }

    /// Poll stderr until a line containing `pattern` appears, or timeout.
    fn wait_for_stderr(&self, pattern: &str, timeout: Duration) -> bool {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if self.stderr_contains(pattern) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }
}

impl Drop for DibsMount {
    fn drop(&mut self) {
        // Kill the process if still alive.
        if self.child.try_wait().ok().flatten().is_none() {
            unsafe {
                libc::kill(self.pid(), libc::SIGKILL);
            }
            let _ = self.child.wait();
        }
        // Force-unmount if still mounted so the temp dir can be cleaned up.
        if is_mounted(self.mountpoint()) {
            let _ = Command::new("umount")
                .args(["-f", self.mountpoint().to_str().unwrap()])
                .status();
        }
    }
}

/// Check whether `path` appears in the output of the `mount` command.
fn is_mounted(path: &Path) -> bool {
    let output = Command::new("mount")
        .output()
        .expect("failed to run mount");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let path_str = path.to_string_lossy();
    stdout
        .lines()
        .any(|line| line.contains(path_str.as_ref()))
}

/// Block until `path` is no longer mounted, or panic on timeout.
fn wait_until_unmounted(path: &Path, timeout: Duration) {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if !is_mounted(path) {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("{:?} still mounted after {:?}", path, timeout);
}

/// Run `dibs unmount <mountpoint>` and return its full output.
fn dibs_unmount(mountpoint: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_dibs"))
        .args(["unmount", mountpoint.to_str().unwrap()])
        .output()
        .expect("failed to run dibs unmount")
}

/// Spawn a `sleep` process whose cwd is inside the mountpoint, making the
/// mount busy from the kernel's perspective.
fn hold_busy(mountpoint: &Path) -> Child {
    Command::new("sleep")
        .arg("3600")
        .current_dir(mountpoint)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn busy-holder")
}

fn kill_child(child: &mut Child) {
    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGKILL);
    }
    let _ = child.wait();
}

// ---------------------------------------------------------------------------
// Scenarios (see SCENARIOS.md)
// ---------------------------------------------------------------------------

/// Scenario 1: ctrl-C when the mount is not busy.
///
/// Expected: dibs unmounts promptly, prints shutdown messages, exits 0.
#[test]
#[ignore]
fn scenario_01_ctrl_c_not_busy() {
    let mut dibs = DibsMount::start();
    dibs.wait_for_mount(Duration::from_secs(5));

    let t0 = Instant::now();
    dibs.send_signal(libc::SIGINT);

    let status = dibs
        .wait_with_timeout(Duration::from_secs(3))
        .expect("dibs did not exit within 3s of SIGINT");

    let elapsed = t0.elapsed();
    assert!(
        elapsed < Duration::from_secs(1),
        "exit took too long: {:?}",
        elapsed,
    );
    assert!(status.success(), "expected exit 0, got {:?}", status);
    assert!(
        dibs.stderr_contains("unmounting (received signal)"),
        "missing 'unmounting' message in stderr:\n{}",
        dibs.stderr_snapshot().join("\n"),
    );
    assert!(
        dibs.stderr_contains("unmounted"),
        "missing 'unmounted' message in stderr:\n{}",
        dibs.stderr_snapshot().join("\n"),
    );
    wait_until_unmounted(dibs.mountpoint(), Duration::from_secs(2));
}

/// Scenario 2: ctrl-C when the mount is busy (a process has its cwd inside).
///
/// Two-phase shutdown: first ctrl-C warns that the mount is busy, second
/// ctrl-C force-unmounts.  The busy process (CWD in mount) prevents a clean
/// unmount, which is detected by the `umount` probe.
#[test]
#[ignore]
fn scenario_02_ctrl_c_busy() {
    let mut dibs = DibsMount::start();
    dibs.wait_for_mount(Duration::from_secs(5));

    let mut busy = hold_busy(dibs.mountpoint());

    // First ctrl-C — should warn about busy mount.
    dibs.send_signal(libc::SIGINT);

    assert!(
        dibs.wait_for_stderr("mount is busy", Duration::from_secs(3)),
        "missing 'mount is busy' warning:\n{}",
        dibs.stderr_snapshot().join("\n"),
    );
    assert!(dibs.is_running(), "dibs should still be running after first ctrl-C");

    // Second ctrl-C — should force unmount.
    dibs.send_signal(libc::SIGINT);

    let status = dibs
        .wait_with_timeout(Duration::from_secs(3))
        .expect("dibs did not exit within 3s of second SIGINT");
    assert!(status.success(), "expected exit 0, got {:?}", status);
    assert!(
        dibs.stderr_contains("force unmounting"),
        "missing 'force unmounting' message:\n{}",
        dibs.stderr_snapshot().join("\n"),
    );

    kill_child(&mut busy);
}

/// Scenario 3: external unmount via `dibs unmount`, mount not busy.
///
/// Expected: unmount command succeeds, dibs process detects the session ended
/// and exits 0.
#[test]
#[ignore]
fn scenario_03_external_unmount_not_busy() {
    let mut dibs = DibsMount::start();
    dibs.wait_for_mount(Duration::from_secs(5));

    let output = dibs_unmount(dibs.mountpoint());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "dibs unmount failed: {}",
        stderr,
    );
    assert!(
        stderr.contains("Successfully unmounted"),
        "unexpected unmount stderr: {}",
        stderr,
    );

    // The dibs mount process should detect the external unmount and exit.
    let status = dibs
        .wait_with_timeout(Duration::from_secs(3))
        .expect("dibs mount process did not exit after external unmount");
    assert!(status.success(), "expected exit 0, got {:?}", status);
    assert!(
        dibs.wait_for_stderr("unmounted", Duration::from_secs(2)),
        "dibs never printed 'unmounted':\n{}",
        dibs.stderr_snapshot().join("\n"),
    );
}

/// Scenario 4: external unmount via `dibs unmount`, mount is busy.
///
/// Expected: unmount command fails with a "busy" message.  dibs keeps running.
/// The mount is unaffected.
#[test]
#[ignore]
fn scenario_04_external_unmount_busy() {
    let mut dibs = DibsMount::start();
    dibs.wait_for_mount(Duration::from_secs(5));

    let mut busy = hold_busy(dibs.mountpoint());

    let output = dibs_unmount(dibs.mountpoint());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "unmount should have failed when busy, but succeeded",
    );
    assert!(
        stderr.contains("busy"),
        "expected 'busy' in stderr, got: {}",
        stderr,
    );

    // dibs should still be running and the mount should still be there.
    assert!(dibs.is_running(), "dibs should still be running");
    assert!(
        is_mounted(dibs.mountpoint()),
        "mount should still be present",
    );

    // Clean up.
    kill_child(&mut busy);
    dibs.send_signal(libc::SIGINT);
    dibs.wait_with_timeout(Duration::from_secs(3));
}

/// Scenario 5: external unmount fails (busy), user fixes it, retry succeeds.
///
/// Expected: first attempt fails with "busy", second attempt succeeds,
/// dibs mount process exits.
#[test]
#[ignore]
fn scenario_05_external_unmount_busy_then_retry() {
    let mut dibs = DibsMount::start();
    dibs.wait_for_mount(Duration::from_secs(5));

    let mut busy = hold_busy(dibs.mountpoint());

    // First attempt — should fail.
    let output = dibs_unmount(dibs.mountpoint());
    assert!(!output.status.success(), "first unmount should have failed");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("busy"), "expected busy message, got: {}", stderr);
    assert!(dibs.is_running(), "dibs should still be running after failed unmount");

    // Release the busy state.
    kill_child(&mut busy);
    // Give the kernel a moment to notice the process is gone.
    std::thread::sleep(Duration::from_millis(500));

    // Second attempt — should succeed.
    let output = dibs_unmount(dibs.mountpoint());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "retry unmount failed: {}",
        stderr,
    );

    // dibs mount process should detect the unmount and exit.
    let status = dibs
        .wait_with_timeout(Duration::from_secs(3))
        .expect("dibs did not exit after retry unmount");
    assert!(status.success(), "expected exit 0, got {:?}", status);
}

/// Scenario 6: external unmount, then ctrl-C before dibs detects it.
///
/// This is a race between the 200ms poll detecting `guard.is_finished()` and
/// the signal waking the pipe.  Either path is fine — the process should exit
/// cleanly regardless.
#[test]
#[ignore]
fn scenario_06_external_unmount_then_ctrl_c() {
    let mut dibs = DibsMount::start();
    dibs.wait_for_mount(Duration::from_secs(5));

    // Unmount externally.
    let output = dibs_unmount(dibs.mountpoint());
    assert!(output.status.success(), "external unmount failed");

    // Immediately race a SIGINT against the poll timeout.
    dibs.send_signal(libc::SIGINT);

    let status = dibs
        .wait_with_timeout(Duration::from_secs(3))
        .expect("dibs did not exit after external unmount + SIGINT");
    assert!(status.success(), "expected exit 0, got {:?}", status);

    // Regardless of which path won the race, "unmounted" should appear.
    assert!(
        dibs.wait_for_stderr("unmounted", Duration::from_secs(2)),
        "dibs never printed 'unmounted':\n{}",
        dibs.stderr_snapshot().join("\n"),
    );
}

/// Scenario 7: ctrl-C during startup, before the mount is ready.
///
/// The signal pipe isn't installed until `wait_for_shutdown` runs (after
/// `spawn_mount2`), so an early SIGINT hits the default handler and kills the
/// process immediately.  Either way — the process must not hang.
#[test]
#[ignore]
fn scenario_07_ctrl_c_during_startup() {
    let mut dibs = DibsMount::start();

    // Don't wait for the mount — send SIGINT immediately.
    dibs.send_signal(libc::SIGINT);

    let _status = dibs
        .wait_with_timeout(Duration::from_secs(3))
        .expect("dibs did not exit after early SIGINT — startup hung");

    // Exit code may or may not be 0 depending on how far startup got.
    // The invariant under test is: it exited and didn't hang.
}

/// Scenario 8: multiple ctrl-C (impatient user).
///
/// Expected: no crash, no double-free, no hang.  Exits cleanly once.
#[test]
#[ignore]
fn scenario_08_multiple_ctrl_c() {
    let mut dibs = DibsMount::start();
    dibs.wait_for_mount(Duration::from_secs(5));

    dibs.send_signal(libc::SIGINT);
    std::thread::sleep(Duration::from_millis(50));
    dibs.send_signal(libc::SIGINT);

    let status = dibs
        .wait_with_timeout(Duration::from_secs(3))
        .expect("dibs did not exit after double SIGINT");

    assert!(status.success(), "expected exit 0, got {:?}", status);
    wait_until_unmounted(dibs.mountpoint(), Duration::from_secs(2));
}

/// Scenario 9: SIGTERM instead of SIGINT.
///
/// Expected: identical behavior to ctrl-C — clean unmount, exit 0.
#[test]
#[ignore]
fn scenario_09_sigterm() {
    let mut dibs = DibsMount::start();
    dibs.wait_for_mount(Duration::from_secs(5));

    let t0 = Instant::now();
    dibs.send_signal(libc::SIGTERM);

    let status = dibs
        .wait_with_timeout(Duration::from_secs(3))
        .expect("dibs did not exit within 3s of SIGTERM");

    let elapsed = t0.elapsed();
    assert!(
        elapsed < Duration::from_secs(1),
        "exit took too long: {:?}",
        elapsed,
    );
    assert!(status.success(), "expected exit 0, got {:?}", status);
    assert!(
        dibs.stderr_contains("unmounting (received signal)"),
        "missing 'unmounting' message",
    );
    assert!(dibs.stderr_contains("unmounted"), "missing 'unmounted' message");
    wait_until_unmounted(dibs.mountpoint(), Duration::from_secs(2));
}

/// Scenario 10: SIGKILL — cannot be caught.
///
/// The process dies immediately.  The stale mount entry may linger — some FUSE
/// implementations (e.g. macFUSE) do NOT auto-cleanup after the process dies.
/// The Drop impl force-unmounts as cleanup.
#[test]
#[ignore]
fn scenario_10_sigkill() {
    let mut dibs = DibsMount::start();
    dibs.wait_for_mount(Duration::from_secs(5));

    dibs.send_signal(libc::SIGKILL);

    let status = dibs
        .wait_with_timeout(Duration::from_secs(3))
        .expect("process did not die from SIGKILL");
    assert!(
        !status.success(),
        "SIGKILL should not produce exit 0, got {:?}",
        status,
    );

    // The mount is now stale (FUSE process is gone).  AutoUnmount behavior
    // varies — macFUSE leaves the stale entry in `mount` output indefinitely.
    // We just verify the mount is non-functional (access fails).
    let probe = std::fs::read_dir(dibs.mountpoint());
    assert!(
        probe.is_err(),
        "stale mount should not be accessible, but read_dir succeeded",
    );
    // Drop impl will force-unmount.
}

/// Scenario 11: ctrl-C with open file handles.
///
/// First ctrl-C should warn about open files and keep running.
/// Second ctrl-C should force unmount and exit.
#[test]
#[ignore]
fn scenario_11_ctrl_c_open_handles() {
    let mut dibs = DibsMount::start();
    dibs.wait_for_mount(Duration::from_secs(5));

    // Open a file through the mount to create an open FUSE handle.
    let _file = File::open(dibs.mountpoint().join("hello.txt"))
        .expect("failed to open hello.txt through mount");

    // First ctrl-C — should warn about open files.
    dibs.send_signal(libc::SIGINT);

    assert!(
        dibs.wait_for_stderr("mount is busy", Duration::from_secs(3)),
        "missing 'mount is busy' warning:\n{}",
        dibs.stderr_snapshot().join("\n"),
    );
    assert!(dibs.is_running(), "dibs should still be running after first ctrl-C");

    // Second ctrl-C — should force unmount.
    dibs.send_signal(libc::SIGINT);

    let status = dibs
        .wait_with_timeout(Duration::from_secs(3))
        .expect("dibs did not exit after second SIGINT");
    assert!(status.success(), "expected exit 0, got {:?}", status);
    assert!(
        dibs.stderr_contains("force unmounting"),
        "missing 'force unmounting' message:\n{}",
        dibs.stderr_snapshot().join("\n"),
    );
}

/// Scenario 12: ctrl-C with open handles, then files close → auto-unmount.
///
/// First ctrl-C warns. Dropping the file handle should trigger automatic
/// clean unmount without needing a second signal.  The periodic `umount`
/// probe detects the mount is no longer busy.
#[test]
#[ignore]
fn scenario_12_ctrl_c_then_files_close() {
    let mut dibs = DibsMount::start();
    dibs.wait_for_mount(Duration::from_secs(5));

    // Open a file through the mount.
    let file = File::open(dibs.mountpoint().join("hello.txt"))
        .expect("failed to open hello.txt through mount");

    // First ctrl-C — should warn.
    dibs.send_signal(libc::SIGINT);

    assert!(
        dibs.wait_for_stderr("mount is busy", Duration::from_secs(3)),
        "missing 'mount is busy' warning:\n{}",
        dibs.stderr_snapshot().join("\n"),
    );
    assert!(dibs.is_running(), "dibs should still be running after first ctrl-C");

    // Close the file handle — mount should become not-busy.
    drop(file);

    // The periodic umount probe (~1s interval) detects the mount is free.
    assert!(
        dibs.wait_for_stderr("all clear", Duration::from_secs(5)),
        "missing 'all clear' message:\n{}",
        dibs.stderr_snapshot().join("\n"),
    );

    let status = dibs
        .wait_with_timeout(Duration::from_secs(3))
        .expect("dibs did not exit after files closed");
    assert!(status.success(), "expected exit 0, got {:?}", status);
}

/// Scenario 13: SIGTERM with open file handles.
///
/// Identical behavior to ctrl-C (scenario 11) — first SIGTERM warns,
/// second SIGTERM forces unmount.
#[test]
#[ignore]
fn scenario_13_sigterm_open_handles() {
    let mut dibs = DibsMount::start();
    dibs.wait_for_mount(Duration::from_secs(5));

    let _file = File::open(dibs.mountpoint().join("hello.txt"))
        .expect("failed to open hello.txt through mount");

    // First SIGTERM — should warn about open files.
    dibs.send_signal(libc::SIGTERM);

    assert!(
        dibs.wait_for_stderr("mount is busy", Duration::from_secs(3)),
        "missing 'mount is busy' warning:\n{}",
        dibs.stderr_snapshot().join("\n"),
    );
    assert!(dibs.is_running(), "dibs should still be running after first SIGTERM");

    // Second SIGTERM — should force unmount.
    dibs.send_signal(libc::SIGTERM);

    let status = dibs
        .wait_with_timeout(Duration::from_secs(3))
        .expect("dibs did not exit after second SIGTERM");
    assert!(status.success(), "expected exit 0, got {:?}", status);
    assert!(
        dibs.stderr_contains("force unmounting"),
        "missing 'force unmounting' message:\n{}",
        dibs.stderr_snapshot().join("\n"),
    );
}
