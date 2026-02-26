use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

/// Threshold for switching from SHA-256 to xxHash (10 MB).
const HASH_THRESHOLD: u64 = 10 * 1024 * 1024;

/// Compute a hash of the file at the given path.
/// Uses SHA-256 for files <= 10MB, xxHash (XXH3-128) for larger files.
/// Returns None if the file doesn't exist or can't be read.
pub fn hash_file(path: &Path) -> io::Result<Vec<u8>> {
    let metadata = std::fs::metadata(path)?;
    let size = metadata.len();

    let mut file = File::open(path)?;

    if size <= HASH_THRESHOLD {
        hash_sha256(&mut file)
    } else {
        hash_xxh3(&mut file)
    }
}

fn hash_sha256(file: &mut File) -> io::Result<Vec<u8>> {
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().to_vec())
}

fn hash_xxh3(file: &mut File) -> io::Result<Vec<u8>> {
    let mut buf = [0u8; 65536];
    let mut total = Vec::new();
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        total.extend_from_slice(&buf[..n]);
    }
    let hash = xxhash_rust::xxh3::xxh3_128(&total);
    Ok(hash.to_be_bytes().to_vec())
}

/// Format a hash as a hex string.
pub fn hash_hex(hash: &[u8]) -> String {
    hash.iter().map(|b| format!("{:02x}", b)).collect()
}
