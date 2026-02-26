/// Passthrough helpers for FUSE operations.
/// These convert between FUSE types and system types.
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuser::FileAttr;

/// Convert libc::stat to fuser::FileAttr.
pub fn stat_to_file_attr(st: &libc::stat) -> FileAttr {
    FileAttr {
        ino: fuser::INodeNo(st.st_ino),
        size: st.st_size as u64,
        blocks: st.st_blocks as u64,
        atime: system_time_from_parts(st.st_atime, st.st_atime_nsec),
        mtime: system_time_from_parts(st.st_mtime, st.st_mtime_nsec),
        ctime: system_time_from_parts(st.st_ctime, st.st_ctime_nsec),
        crtime: system_time_from_parts(st.st_birthtime, st.st_birthtime_nsec),
        kind: mode_to_filetype(st.st_mode as u32),
        perm: (st.st_mode as u32 & 0o7777) as u16,
        nlink: st.st_nlink as u32,
        uid: st.st_uid,
        gid: st.st_gid,
        rdev: st.st_rdev as u32,
        blksize: st.st_blksize as u32,
        flags: st.st_flags,
    }
}

fn system_time_from_parts(sec: i64, nsec: i64) -> SystemTime {
    if sec >= 0 {
        UNIX_EPOCH + Duration::new(sec as u64, nsec as u32)
    } else {
        UNIX_EPOCH
    }
}

pub fn mode_to_filetype(mode: u32) -> fuser::FileType {
    let fmt = mode & (libc::S_IFMT as u32);
    match fmt {
        x if x == libc::S_IFREG as u32 => fuser::FileType::RegularFile,
        x if x == libc::S_IFDIR as u32 => fuser::FileType::Directory,
        x if x == libc::S_IFLNK as u32 => fuser::FileType::Symlink,
        x if x == libc::S_IFBLK as u32 => fuser::FileType::BlockDevice,
        x if x == libc::S_IFCHR as u32 => fuser::FileType::CharDevice,
        x if x == libc::S_IFIFO as u32 => fuser::FileType::NamedPipe,
        x if x == libc::S_IFSOCK as u32 => fuser::FileType::Socket,
        _ => fuser::FileType::RegularFile,
    }
}

/// Perform lstat() on a path.
pub fn lstat(path: &Path) -> std::io::Result<libc::stat> {
    let c_path = path_to_cstring(path)?;
    unsafe {
        let mut st: libc::stat = std::mem::zeroed();
        if libc::lstat(c_path.as_ptr(), &mut st) == 0 {
            Ok(st)
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
}

/// Perform fstat() on a file descriptor.
pub fn fstat(fd: i32) -> std::io::Result<libc::stat> {
    unsafe {
        let mut st: libc::stat = std::mem::zeroed();
        if libc::fstat(fd, &mut st) == 0 {
            Ok(st)
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
}

pub fn path_to_cstring(path: &Path) -> std::io::Result<std::ffi::CString> {
    std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "path contains null byte"))
}
