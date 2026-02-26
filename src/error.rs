use thiserror::Error;

#[derive(Error, Debug)]
pub enum DibsError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("CAS conflict on {path}: expected hash {expected}, found {actual}")]
    CasConflict {
        path: String,
        expected: String,
        actual: String,
    },

    #[error("File not tracked: {0}")]
    NotTracked(String),

    #[error("Write ownership conflict on {path}: owned by handle {owner}")]
    WriteOwnership { path: String, owner: u64 },

    #[error("Mount error: {0}")]
    Mount(String),

    #[error("Configuration error: {0}")]
    Config(String),
}

pub type Result<T> = std::result::Result<T, DibsError>;
