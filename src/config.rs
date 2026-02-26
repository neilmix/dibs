use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "dibs", about = "FUSE filesystem with optimistic concurrency control")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Mount a dibs filesystem
    Mount {
        /// Path to the backing directory
        backing: PathBuf,

        /// Path to the mount point
        mountpoint: PathBuf,

        /// Session identifier for logging
        #[arg(long)]
        session_id: Option<String>,

        /// Log file path
        #[arg(long, default_value = "/tmp/dibs.log")]
        log_file: PathBuf,

        /// Minutes before evicting idle CAS entries
        #[arg(long, default_value_t = 60)]
        eviction_minutes: u64,

        /// Save rejected write contents to .dibs/conflicts/
        #[arg(long)]
        save_conflicts: bool,

        /// Fall back to read-only on CAS errors instead of EIO
        #[arg(long)]
        readonly_fallback: bool,

        /// Run in foreground (don't daemonize)
        #[arg(short, long)]
        foreground: bool,
    },
    /// Unmount a dibs filesystem
    Unmount {
        /// Path to the mount point
        mountpoint: PathBuf,
    },
}

#[derive(Debug, Clone)]
pub struct DibsConfig {
    pub backing: PathBuf,
    pub mountpoint: PathBuf,
    pub session_id: String,
    pub log_file: PathBuf,
    pub eviction_minutes: u64,
    pub save_conflicts: bool,
    pub readonly_fallback: bool,
    pub foreground: bool,
}
