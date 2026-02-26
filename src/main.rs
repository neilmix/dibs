use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::Arc;

use clap::Parser;
use tracing::{error, info};
use tracing_subscriber::fmt;
use tracing_subscriber::prelude::*;

use dibs::config::{Cli, Command, DibsConfig};
use dibs::fs::DibsFs;

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

/// Block until SIGINT/SIGTERM is received or the FUSE session exits on its own
/// (e.g. external `umount`). Returns `true` if woken by a signal, `false` if the
/// session ended independently.
fn wait_for_shutdown(guard: &std::thread::JoinHandle<std::io::Result<()>>) -> bool {
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

    // Poll the signal pipe with a timeout so we can also notice when the FUSE
    // background thread exits (external unmount). The 200ms timeout means up to
    // 200ms latency detecting external unmount. On that path, the DibsFs may
    // already be dropped by the FUSE thread, so the eviction thread's cas_ptr
    // is dangling — caller must stop eviction ASAP after this returns false.
    let received_signal = loop {
        let mut pfd = libc::pollfd {
            fd: pipe_fds[0],
            events: libc::POLLIN,
            revents: 0,
        };
        let ret = unsafe { libc::poll(&mut pfd, 1, 200) }; // 200 ms timeout
        if ret > 0 {
            let mut buf = [0u8; 1];
            unsafe {
                libc::read(pipe_fds[0], buf.as_mut_ptr() as *mut libc::c_void, 1);
            }
            break true;
        }
        if guard.is_finished() {
            break false;
        }
    };

    SIGNAL_PIPE.store(-1, Ordering::Relaxed);
    unsafe {
        libc::close(pipe_fds[0]);
        libc::close(pipe_fds[1]);
    }

    received_signal
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

            let subscriber = tracing_subscriber::registry()
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
            // TODO: UB — cas_ptr is taken here but dibsfs is moved into spawn_mount2() below,
            // invalidating this pointer. CasTable should be behind an Arc to get a stable address.
            let shutdown = Arc::new(AtomicBool::new(false));
            let cas_ptr = &dibsfs.cas_table as *const _ as *const dibs::state::hash_table::CasTable;
            let eviction_handle = dibs::state::eviction::start_eviction_thread(
                cas_ptr,
                eviction_minutes,
                shutdown.clone(),
            );

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
                        match fuser::spawn_mount2(
                            DibsFs::new(retry_config),
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

            let received_signal = wait_for_shutdown(&session.guard);

            // Stop the eviction thread BEFORE joining the session. Joining the
            // session drops DibsFs (and its CasTable), so the eviction thread's
            // raw pointer would dangle if it were still running at that point.
            shutdown.store(true, Ordering::Relaxed);
            let _ = eviction_handle.join();

            if received_signal {
                eprintln!("dibs: unmounting (received signal)...");
                if let Err(e) = session.umount_and_join() {
                    error!("Error during unmount: {}", e);
                }
            } else {
                // FUSE session ended on its own (external umount).
                if let Err(e) = session.join() {
                    error!("Error joining FUSE session: {}", e);
                }
            }

            eprintln!("dibs: unmounted {}", mountpoint.display());
        }
        Command::Unmount { mountpoint } => {
            unmount(&mountpoint);
        }
    }
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
