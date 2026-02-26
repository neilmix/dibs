# CAS-FS: Compare-and-Swap Filesystem

## Specification v1.0

### Purpose

CAS-FS is a FUSE filesystem that enforces optimistic concurrency control over a backing directory. It is designed for environments where multiple AI coding agents operate concurrently on the same project. The filesystem ensures that no agent can silently overwrite another agent's changes by requiring every write to present a valid content hash obtained from a prior read.

---

### Core Concepts

**Backing directory**: The real directory on disk containing the project files. CAS-FS does not modify the structure or format of files in the backing directory. Files stored there are always plain files, readable by any tool.

**Mount point**: The FUSE-mounted directory through which agents access files. All agent file access MUST go through the mount point. The backing directory should be permission-restricted so agents cannot write to it directly.

**Content hash**: A SHA-256 hash of a file's contents at the time of read. This hash serves as the concurrency token. It is stored and tracked internally by the filesystem; it is NOT embedded in the file contents or stored as extended attributes on the backing files.

**Session**: An optional identifier associated with a mounted agent's activity, passed as a mount option. Used for logging and diagnostics. Not required for correctness.

---

### Architecture

```
Agent A ──► /mnt/project (FUSE mount) ──► CAS-FS daemon ──► /home/user/project (backing dir)
Agent B ──► /mnt/project (FUSE mount) ──┘
```

A single CAS-FS daemon serves all agents through one mount point. Concurrency control happens inside the daemon. The daemon maintains an in-memory hash table mapping file paths to their last-known content hashes.

---

### Filesystem Operations

#### READ operations (open, read, readdir, stat, getattr)

All read operations pass through to the backing directory transparently with standard POSIX semantics. No special behavior is required.

When a file is opened for reading (O_RDONLY), the daemon:
1. Reads the file content from the backing directory.
2. Computes SHA-256 of the content.
3. Stores the mapping `(path, hash)` in its internal hash table, overwriting any previous entry for that path.
4. Returns the file content to the caller normally.

When a file is opened with O_RDWR or O_WRONLY, the daemon MUST also perform the read-and-hash step above before allowing the file descriptor to be used for writing. This ensures a hash is always established before a write can occur.

#### WRITE operations (write, truncate)

When a write is attempted on an open file descriptor, the daemon:
1. Looks up the hash currently stored for that path in the internal hash table.
2. Reads the current content of the backing file and computes its current hash.
3. Compares the stored hash (from step 1) with the current hash (from step 2).
4. **If hashes match**: The file has not been modified since this file descriptor's associated read. The write proceeds. After the write completes, the daemon computes the new hash of the written content and updates the hash table entry for the path.
5. **If hashes do not match**: The file has been modified by another agent since this file descriptor read it. The write is REJECTED by returning `EIO` (I/O error). The daemon logs the conflict including the path, the expected hash, the actual hash, and the session IDs if available.

This comparison MUST be atomic with respect to other write operations on the same path. Use a per-path mutex or equivalent synchronization.

#### CREATE operations (create, mknod)

Creating a new file is always permitted. The daemon:
1. Creates the file in the backing directory.
2. Computes the hash of the initial content (empty string hash for zero-length files).
3. Stores the mapping in the hash table.

If two agents race to create the same path, standard POSIX O_EXCL semantics apply if the agent uses them. Otherwise, last-create-wins is acceptable for creation — the concurrency protection matters for subsequent edits.

#### DELETE operations (unlink, rmdir)

Delete operations apply the same CAS check:
1. The daemon looks up the stored hash for the path.
2. Computes the current hash of the backing file.
3. If they match, the delete proceeds and the hash table entry is removed.
4. If they do not match, the delete is rejected with `EIO`.

If no hash is stored for the path (the deleting agent never read the file), the delete is REJECTED. An agent must read a file before it can delete it. This prevents an agent from deleting a file that another agent is actively editing.

#### RENAME operations (rename)

Rename is treated as a compound operation:
1. CAS check on the source path (agent must have read it, hash must match).
2. If a file exists at the destination path, CAS check on the destination as well.
3. If all checks pass, the rename proceeds in the backing directory and the hash table is updated (old entry removed, new entry created with the same hash).
4. If any check fails, the rename is rejected with `EIO`.

#### DIRECTORY operations (mkdir, rmdir, readdir)

Directory creation and listing are always permitted with no CAS checks. Directory removal follows normal POSIX semantics (must be empty).

#### SYMLINK and HARDLINK operations

Symlink creation is permitted. Hardlinks are NOT supported (return `ENOTSUP`) because they would create multiple paths to the same inode, complicating hash tracking.

#### CHMOD, CHOWN, UTIMENS

These metadata operations pass through to the backing directory without CAS checks. They do not modify file content and therefore do not affect content hashes.

---

### Hash Table Management

The hash table is an in-memory data structure mapping `(absolute_path) → (hash, timestamp)`.

**Eviction**: Entries for files not accessed in the last 60 minutes MAY be evicted. This is a memory optimization only. If an entry is evicted and a write is attempted, the write is rejected (no stored hash to compare against), forcing the agent to re-read first. This is the correct and safe behavior.

**Staleness**: The hash table entry for a path is invalidated (removed) whenever:
- A successful write updates the file through the FUSE layer (entry is replaced with new hash).
- The backing file is modified outside the FUSE layer (detected via inotify on the backing directory — see External Modifications below).
- The entry is evicted due to age.

**Concurrency**: The hash table MUST support concurrent access. Use a per-path lock (not a global lock) to allow parallel operations on different files.

---

### External Modifications

The backing directory may be modified outside the FUSE layer (e.g., by `git checkout`, a build tool, or the user directly). The daemon MUST watch the backing directory using inotify (or equivalent) and:

1. When a backing file is modified externally, invalidate (remove) its hash table entry.
2. The next agent write to that path will fail the CAS check (no stored hash), forcing a re-read. This is the desired behavior.
3. Optionally, log external modifications for debugging.

---

### Error Reporting

When a write is rejected due to a CAS conflict, the daemon MUST:

1. Return `EIO` to the calling process.
2. Write a structured log entry to a configurable log file (default: `/tmp/casfs.log`) containing:
   - Timestamp (ISO 8601)
   - File path
   - Expected hash (from the writing agent's read)
   - Actual hash (current file state)
   - PID of the rejected process
   - Session ID if available
3. Optionally, create a conflict marker file at `<mount_point>/.casfs/conflicts/<filename>.<timestamp>` containing the rejected write's content. This allows the agent or user to recover the rejected changes. This directory is virtual (not present in the backing directory).

---

### Virtual Control Directory

The daemon exposes a virtual directory at `<mount_point>/.casfs/` that does NOT exist in the backing directory. This directory provides:

- `.casfs/status` — A read-only file returning JSON with current daemon state: number of tracked files, active locks, recent conflicts.
- `.casfs/conflicts/` — Directory containing rejected write contents (see Error Reporting above).
- `.casfs/locks` — A read-only file returning JSON listing all files with active hash entries, their hashes, and the timestamps of last read. This allows agents or tooling to see what files are "in play."

Writes to any file under `.casfs/` return `EACCES` (permission denied), except:
- Writing the string `"clear\n"` to `.casfs/conflicts/<filename>` deletes that conflict record.

---

### Mount Options

The FUSE filesystem accepts the following mount options:

| Option | Type | Default | Description |
|---|---|---|---|
| `backing_dir` | string | required | Absolute path to the backing directory. |
| `session_id` | string | `""` | Optional identifier for this mount session (for logging). |
| `log_file` | string | `/tmp/casfs.log` | Path to the conflict log file. |
| `eviction_minutes` | int | `60` | Minutes before unused hash entries are evicted. |
| `save_conflicts` | bool | `true` | Whether to save rejected write content to `.casfs/conflicts/`. |
| `readonly_fallback` | bool | `false` | If true, on CAS conflict, silently make the fd read-only instead of returning EIO. Some tools handle EIO poorly. |

---

### Operational Notes

**Permissions enforcement**: The backing directory should be owned by a dedicated user (e.g., `casfs`) and not writable by the agents' user. The FUSE daemon runs as `casfs` (or root) and proxies all operations. Agents access files through the mount point under FUSE's permission model. This prevents agents from bypassing CAS-FS by writing directly to the backing directory.

**Performance**: The CAS check on write requires reading the current backing file content and computing SHA-256. For large files, this adds latency. Acceptable tradeoffs:
- Files under 10 MB: always perform full CAS check.
- Files over 10 MB: use a fast hash (xxHash) instead of SHA-256, or compare mtime+size as a preliminary check before full hash. Document which approach is chosen.

**Signal handling**: On SIGTERM or SIGINT, the daemon should cleanly unmount the FUSE filesystem, flush the conflict log, and exit. On SIGHUP, reload configuration (log file path, eviction time) without unmounting.

**File descriptor semantics**: The hash is associated with the file descriptor's open event, not with each individual read call. If an agent opens a file, reads it, then another agent modifies and closes the file, then the first agent writes — the write MUST be rejected. The hash was established at open/first-read time and the file has since changed.

---

### Implementation Guidance

**Recommended language**: Rust (with the `fuser` crate) or C (with `libfuse`). Rust is preferred for memory safety in a concurrent daemon.

**Key data structures**:
- `HashMap<PathBuf, FileState>` where `FileState = { hash: [u8; 32], timestamp: Instant, conflict_content: Option<Vec<u8>> }`
- `HashMap<u64, PathBuf>` mapping file handle IDs to paths for tracking which fd corresponds to which CAS entry.
- Per-path `Mutex` or `RwLock` for the CAS check-and-write atomic section.

**Testing requirements**:
1. Two concurrent writers to the same file. First write succeeds, second is rejected.
2. Read-write-read-write cycle by a single agent. Both writes succeed (hash updates correctly).
3. External modification of backing file. Next agent write is rejected.
4. Agent reads file A, agent reads file B, writes file B, writes file A. Both writes succeed (different files, no conflict).
5. Agent A reads file, agent B reads same file, agent A writes (succeeds), agent B writes (rejected).
6. File creation by two agents with different names. Both succeed.
7. Delete of a file currently being edited by another agent. Rejected.
8. Large file (>10 MB) performance is acceptable (write completes in under 500ms).
9. Daemon handles 1000+ tracked files without degradation.
10. Clean shutdown preserves no stale mount points.

---

### Out of Scope

The following are explicitly NOT part of this specification:
- Conflict resolution or merging. CAS-FS only detects conflicts and rejects the losing write. The agent or user decides what to do.
- Network or distributed operation. CAS-FS operates on a single machine.
- Versioning or history. CAS-FS does not store prior versions (use git for that).
- Encryption or access control beyond basic POSIX permissions.
