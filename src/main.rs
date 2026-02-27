use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::Arc;

use clap::Parser;
use tracing::{error, info};
use tracing_subscriber::fmt;
use tracing_subscriber::prelude::*;

use dibs::config::{Cli, Command, DibsConfig};
use dibs::fs::handles::HandleTable;
use dibs::fs::DibsFs;

use std::path::Path;

/// Write-end of the self-pipe used for signal notification.
static SIGNAL_PIPE: AtomicI32 = AtomicI32::new(-1);

extern "C" fn signal_handler(_sig: libc::c_int) {
    let fd = SIGNAL_PIPE.load(Ordering::Relaxed);
    if fd >= 0 {
        unsafe {
            libc::write(fd, [0u8].as_ptr() as *const libc::c_void, 1);
        }
    }
}

enum ShutdownAction {
    /// Second signal — force unmount.
    ForceUnmount,
    /// FUSE session ended on its own, or first-signal probe unmount succeeded.
    ExternalUnmount,
}

/// Attempt a regular (non-forced) unmount. Returns true if the mount was
/// successfully removed — i.e. the mount was not busy.
fn try_unmount(mountpoint: &Path) -> bool {
    std::process::Command::new("umount")
        .arg(mountpoint)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Block until shutdown conditions are met. Implements two-phase ctrl-C:
/// - First signal when mount is not busy: immediate clean unmount.
/// - First signal when mount is busy (open handles, CWD, etc.): warn and wait.
/// - Second signal: force unmount.
/// - FUSE session exits on its own: external unmount.
///
/// Mount busyness is probed via a regular (non-forced) `umount` call — this
/// catches both FUSE file handles and kernel VFS references (e.g. CWD).
fn wait_for_shutdown(
    guard: &std::thread::JoinHandle<std::io::Result<()>>,
    file_handles: &HandleTable,
    mountpoint: &Path,
) -> ShutdownAction {
    let mut pipe_fds = [0 as libc::c_int; 2];
    assert_eq!(
        unsafe { libc::pipe(pipe_fds.as_mut_ptr()) },
        0,
        "failed to create signal pipe"
    );

    SIGNAL_PIPE.store(pipe_fds[1], Ordering::Relaxed);

    unsafe {
        use nix::sys::signal::{signal, SigHandler, Signal};
        signal(Signal::SIGINT, SigHandler::Handler(signal_handler)).ok();
        signal(Signal::SIGTERM, SigHandler::Handler(signal_handler)).ok();
    }

    let mut first_signal_received = false;
    let mut poll_ticks: u32 = 0;

    // Poll the signal pipe with a timeout so we can also notice when the FUSE
    // background thread exits (external unmount). The 200ms timeout means up to
    // 200ms latency detecting external unmount or handle count changes.
    let action = loop {
        let mut pfd = libc::pollfd {
            fd: pipe_fds[0],
            events: libc::POLLIN,
            revents: 0,
        };
        let ret = unsafe { libc::poll(&mut pfd, 1, 200) }; // 200 ms timeout

        if ret > 0 {
            // Drain one byte from the signal pipe.
            let mut buf = [0u8; 1];
            unsafe {
                libc::read(pipe_fds[0], buf.as_mut_ptr() as *mut libc::c_void, 1);
            }

            if first_signal_received {
                // Second signal — force unmount.
                eprintln!("dibs: force unmounting...");
                break ShutdownAction::ForceUnmount;
            }

            // First signal — check if the session already ended (race with
            // external unmount that happened just before the signal).
            if guard.is_finished() {
                break ShutdownAction::ExternalUnmount;
            }

            // Probe actual mount busyness via system umount. If it succeeds,
            // the mount was cleanly removed — the FUSE session will notice.
            if try_unmount(mountpoint) {
                eprintln!("dibs: unmounting (received signal)...");
                break ShutdownAction::ExternalUnmount;
            }

            // Mount is busy — warn and wait.
            first_signal_received = true;
            let open_files = file_handles.list_open();
            if open_files.is_empty() {
                eprintln!(
                    "dibs: mount is busy — processes are using the mountpoint"
                );
            } else {
                eprintln!(
                    "dibs: mount is busy — {} open file(s):",
                    open_files.len(),
                );
                let display_cap = 10;
                for info in open_files.iter().take(display_cap) {
                    eprintln!(
                        "  {}  (SID {})",
                        info.path.display(),
                        info.sid,
                    );
                }
                if open_files.len() > display_cap {
                    eprintln!("  and {} more...", open_files.len() - display_cap);
                }
            }
            eprintln!(
                "Close open files to unmount cleanly, or press ctrl-C again to force unmount."
            );
            continue;
        }

        // Poll timeout — check for external unmount or mount release.
        if guard.is_finished() {
            break ShutdownAction::ExternalUnmount;
        }

        if first_signal_received {
            poll_ticks += 1;
            // Probe every ~1 second (5 ticks * 200ms) to avoid spawning
            // umount too frequently.
            if poll_ticks % 5 == 0 && try_unmount(mountpoint) {
                eprintln!("dibs: all clear, unmounting...");
                break ShutdownAction::ExternalUnmount;
            }
        }
    };

    SIGNAL_PIPE.store(-1, Ordering::Relaxed);
    unsafe {
        libc::close(pipe_fds[0]);
        libc::close(pipe_fds[1]);
    }

    action
}

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Command::Mount {
            backing,
            mountpoint,
            session_id,
            log_file,
            eviction_minutes,
            save_conflicts,
            readonly_fallback,
            foreground,
        } => {
            let backing = std::fs::canonicalize(&backing).unwrap_or_else(|e| {
                eprintln!("Error: backing directory {:?}: {}", backing, e);
                std::process::exit(1);
            });

            if !backing.is_dir() {
                eprintln!("Error: backing path is not a directory: {:?}", backing);
                std::process::exit(1);
            }

            // Create mountpoint if it doesn't exist
            if !mountpoint.exists() {
                if let Err(e) = std::fs::create_dir_all(&mountpoint) {
                    eprintln!("Error creating mountpoint {:?}: {}", mountpoint, e);
                    std::process::exit(1);
                }
            }

            let mountpoint = std::fs::canonicalize(&mountpoint).unwrap_or_else(|e| {
                eprintln!("Error: mountpoint {:?}: {}", mountpoint, e);
                std::process::exit(1);
            });

            // Check for a stale FUSE mount left behind by a previous crash or
            // forced kill.  macFUSE does not auto-cleanup these.
            if is_stale_fuse_mount(&mountpoint) {
                eprintln!(
                    "Error: {} is a stale FUSE mount (previous dibs session didn't clean up).\n\
                     Fix with:  umount -f {}",
                    mountpoint.display(),
                    mountpoint.display(),
                );
                std::process::exit(1);
            }

            let sid = session_id.unwrap_or_else(|| {
                format!("dibs-{}", std::process::id())
            });

            // Set up logging
            let log_dir = log_file.parent().unwrap_or_else(|| std::path::Path::new("/tmp"));
            let log_name = log_file
                .file_name()
                .unwrap_or_else(|| std::ffi::OsStr::new("dibs.log"));
            let file_appender = tracing_appender::rolling::never(log_dir, log_name);
            let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

            let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

            let subscriber = tracing_subscriber::registry()
                .with(env_filter)
                .with(
                    fmt::layer()
                        .with_writer(non_blocking)
                        .with_ansi(false)
                        .with_target(false),
                )
                .with(
                    fmt::layer()
                        .with_writer(std::io::stderr)
                        .with_target(false),
                );
            tracing::subscriber::set_global_default(subscriber)
                .expect("Failed to set tracing subscriber");

            let log_file_for_retry = log_file.clone();
            let config = DibsConfig {
                backing: backing.clone(),
                mountpoint: mountpoint.clone(),
                session_id: sid.clone(),
                log_file,
                eviction_minutes,
                save_conflicts,
                readonly_fallback,
                foreground,
            };

            info!(
                "dibs starting: session={}, backing={}, mountpoint={}",
                sid,
                backing.display(),
                mountpoint.display()
            );

            let dibsfs = DibsFs::new(config);

            // Start eviction thread
            let shutdown = Arc::new(AtomicBool::new(false));
            let cas_arc = Arc::clone(&dibsfs.cas_table);
            let eviction_handle = dibs::state::eviction::start_eviction_thread(
                cas_arc,
                eviction_minutes,
                shutdown.clone(),
            );

            // Clone the file_handles Arc so we can query open handles from main
            // after DibsFs is moved into the FUSE session.
            let mut file_handles_arc = Arc::clone(&dibsfs.file_handles);

            // Mount configuration
            let mut fuse_config = fuser::Config::default();
            fuse_config.mount_options = vec![
                fuser::MountOption::FSName("dibs".to_string()),
                fuser::MountOption::AutoUnmount,
                fuser::MountOption::DefaultPermissions,
            ];
            fuse_config.acl = fuser::SessionACL::All;

            info!("Mounting dibs filesystem...");

            // Spawn FUSE session in background thread
            let session = match fuser::spawn_mount2(dibsfs, &mountpoint, &fuse_config) {
                Ok(session) => session,
                Err(e) => {
                    if e.raw_os_error() == Some(libc::EPERM)
                        || e.to_string().contains("allow_other")
                    {
                        fuse_config.acl = fuser::SessionACL::Owner;
                        info!("Retrying mount without allow_other...");
                        let retry_config = DibsConfig {
                            backing: backing.clone(),
                            mountpoint: mountpoint.clone(),
                            session_id: sid.clone(),
                            log_file: log_file_for_retry,
                            eviction_minutes,
                            save_conflicts,
                            readonly_fallback,
                            foreground,
                        };
                        let retry_dibsfs = DibsFs::new(retry_config);
                        file_handles_arc = Arc::clone(&retry_dibsfs.file_handles);
                        match fuser::spawn_mount2(
                            retry_dibsfs,
                            &mountpoint,
                            &fuse_config,
                        ) {
                            Ok(session) => session,
                            Err(e) => {
                                error!("Failed to mount: {}", e);
                                std::process::exit(1);
                            }
                        }
                    } else {
                        error!("Failed to mount: {}", e);
                        std::process::exit(1);
                    }
                }
            };

            info!("dibs mounted at {}", mountpoint.display());

            let action = wait_for_shutdown(&session.guard, &file_handles_arc, &mountpoint);

            // Stop the eviction thread before joining the session for clean shutdown.
            shutdown.store(true, Ordering::Relaxed);
            let _ = eviction_handle.join();

            match action {
                ShutdownAction::ForceUnmount => {
                    if let Err(e) = session.umount_and_join() {
                        error!("Error during unmount, trying force unmount: {}", e);
                        let mp = mountpoint.to_string_lossy();
                        let _ = std::process::Command::new("umount")
                            .args(["-f", &*mp])
                            .status();
                    }
                }
                ShutdownAction::ExternalUnmount => {
                    if let Err(e) = session.join() {
                        error!("Error joining FUSE session: {}", e);
                    }
                }
            }

            eprintln!("dibs: unmounted {}", mountpoint.display());
        }
        Command::Unmount { mountpoint } => {
            unmount(&mountpoint);
        }
    }
}

/// Check if `path` is a stale FUSE mount: it appears in `mount` output as a
/// fuse/macfuse volume but is no longer functional (readdir fails).
fn is_stale_fuse_mount(path: &std::path::Path) -> bool {
    let output = match std::process::Command::new("mount").output() {
        Ok(o) => o,
        Err(_) => return false,
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let path_str = path.to_string_lossy();
    let is_fuse_mount = stdout.lines().any(|line| {
        line.contains(path_str.as_ref()) && (line.contains("fuse") || line.contains("macfuse"))
    });
    if !is_fuse_mount {
        return false;
    }
    // It's listed as a FUSE mount — check if it's actually functional.
    std::fs::read_dir(path).is_err()
}

fn unmount(mountpoint: &PathBuf) {
    let mountpoint = std::fs::canonicalize(mountpoint).unwrap_or_else(|e| {
        eprintln!("Error: mountpoint {:?}: {}", mountpoint, e);
        std::process::exit(1);
    });
    let mp = mountpoint.to_string_lossy();
    eprintln!("Unmounting {}...", mp);

    // Try umount first
    let output = std::process::Command::new("umount")
        .arg(&*mp)
        .output();

    if matches!(&output, Ok(o) if o.status.success()) {
        eprintln!("Successfully unmounted {}", mp);
        return;
    }

    if let Ok(ref o) = output {
        let stderr = String::from_utf8_lossy(&o.stderr);
        if stderr.contains("busy") {
            eprintln!(
                "Mount point is busy. Make sure no shells or processes are using {}, then try again.",
                mp
            );
            std::process::exit(1);
        }
    }

    // Try diskutil unmount (macOS)
    let output = std::process::Command::new("diskutil")
        .args(["unmount", &*mp])
        .output();

    if matches!(&output, Ok(o) if o.status.success()) {
        eprintln!("Successfully unmounted {}", mp);
        return;
    }

    if let Ok(ref o) = output {
        let stderr = String::from_utf8_lossy(&o.stderr);
        let stdout = String::from_utf8_lossy(&o.stdout);
        if stderr.contains("busy") || stdout.contains("busy") {
            eprintln!(
                "Mount point is busy. Make sure no shells or processes are using {}, then try again.",
                mp
            );
            std::process::exit(1);
        }
    }

    // Force unmount as last resort
    let status = std::process::Command::new("umount")
        .args(["-f", &*mp])
        .status();

    if matches!(status, Ok(s) if s.success()) {
        eprintln!("Successfully unmounted {} (forced)", mp);
        return;
    }

    eprintln!("Failed to unmount {}. Try: sudo umount -f {}", mp, mp);
    std::process::exit(1);
}
