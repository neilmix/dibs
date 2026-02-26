use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use clap::Parser;
use tracing::{error, info};
use tracing_subscriber::fmt;
use tracing_subscriber::prelude::*;

use dibs::config::{Cli, Command, DibsConfig};
use dibs::fs::DibsFs;

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
            // TODO: UB — cas_ptr is taken here but dibsfs is moved into mount2() below,
            // invalidating this pointer. CasTable should be behind an Arc to get a stable address.
            let shutdown = Arc::new(AtomicBool::new(false));
            let cas_ptr = &dibsfs.cas_table as *const _ as *const dibs::state::hash_table::CasTable;
            let eviction_handle = dibs::state::eviction::start_eviction_thread(
                cas_ptr,
                eviction_minutes,
                shutdown.clone(),
            );

            // Set up signal handling for clean unmount
            let mp_for_signal = mountpoint.clone();
            let shutdown_for_signal = shutdown.clone();
            ctrlc_handler(mp_for_signal, shutdown_for_signal);

            // Mount configuration
            let mut fuse_config = fuser::Config::default();
            fuse_config.mount_options = vec![
                fuser::MountOption::FSName("dibs".to_string()),
                fuser::MountOption::AutoUnmount,
                fuser::MountOption::DefaultPermissions,
            ];
            fuse_config.acl = fuser::SessionACL::All;

            info!("Mounting dibs filesystem...");

            // Mount and run
            match fuser::mount2(dibsfs, &mountpoint, &fuse_config) {
                Ok(()) => {
                    info!("dibs filesystem unmounted cleanly");
                }
                Err(e) => {
                    // If AllowOther/ACL fails, retry with default ACL (owner only)
                    if e.raw_os_error() == Some(libc::EPERM) || e.to_string().contains("allow_other") {
                        fuse_config.acl = fuser::SessionACL::Owner;
                        info!("Retrying mount without allow_other...");
                        match fuser::mount2(dibsfs_retry(&backing, &mountpoint, &sid, eviction_minutes, save_conflicts, readonly_fallback, foreground), &mountpoint, &fuse_config) {
                            Ok(()) => {
                                info!("dibs filesystem unmounted cleanly");
                            }
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
            }

            // Signal eviction thread to stop
            shutdown.store(true, Ordering::Relaxed);
            let _ = eviction_handle.join();
        }
        Command::Unmount { mountpoint } => {
            unmount(&mountpoint);
        }
    }
}

fn dibsfs_retry(
    backing: &PathBuf,
    mountpoint: &PathBuf,
    session_id: &str,
    eviction_minutes: u64,
    save_conflicts: bool,
    readonly_fallback: bool,
    foreground: bool,
) -> DibsFs {
    let config = DibsConfig {
        backing: backing.clone(),
        mountpoint: mountpoint.clone(),
        session_id: session_id.to_string(),
        log_file: PathBuf::from("/tmp/dibs.log"),
        eviction_minutes,
        save_conflicts,
        readonly_fallback,
        foreground,
    };
    DibsFs::new(config)
}

fn unmount(mountpoint: &PathBuf) {
    let mp = mountpoint.to_string_lossy();
    eprintln!("Unmounting {}...", mp);

    // Try fusermount first (Linux), then umount (macOS)
    let status = std::process::Command::new("umount")
        .arg(&*mp)
        .status();

    match status {
        Ok(s) if s.success() => {
            eprintln!("Successfully unmounted {}", mp);
        }
        _ => {
            // Try diskutil on macOS
            let status = std::process::Command::new("diskutil")
                .args(["unmount", &*mp])
                .status();
            match status {
                Ok(s) if s.success() => {
                    eprintln!("Successfully unmounted {}", mp);
                }
                _ => {
                    eprintln!("Failed to unmount {}. Try: sudo umount -f {}", mp, mp);
                    std::process::exit(1);
                }
            }
        }
    }
}

fn ctrlc_handler(_mountpoint: PathBuf, _shutdown: Arc<AtomicBool>) {
    let _ = std::thread::Builder::new()
        .name("dibs-signal".to_string())
        .spawn(move || {
            use nix::sys::signal::{self, SigHandler, Signal};

            unsafe {
                signal::signal(Signal::SIGTERM, SigHandler::Handler(signal_handler))
                    .ok();
                signal::signal(Signal::SIGINT, SigHandler::Handler(signal_handler))
                    .ok();
            }
        });
}

static SIGNAL_RECEIVED: AtomicBool = AtomicBool::new(false);

extern "C" fn signal_handler(_sig: libc::c_int) {
    SIGNAL_RECEIVED.store(true, Ordering::Relaxed);
}
