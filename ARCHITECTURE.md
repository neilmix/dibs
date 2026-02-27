# dibs Architecture

dibs is a FUSE filesystem that sits between agents and a shared project directory. Every file operation passes through dibs, which tracks content hashes and rejects writes that would silently overwrite another agent's changes.

```
 Agent A (terminal 1)          Agent B (terminal 2)
        |                              |
        v                              v
  +-----------mount point-----------+
  |            dibs (FUSE)          |
  +--------backing directory--------+
  |     /path/to/project (real fs)  |
  +----------------------------------+
```

Agents read and write files through the mount point. dibs forwards every operation to the backing directory while maintaining a hash table that tracks file contents. If agent B tries to write a file that agent A has already changed since B last read it, dibs returns `EIO` and the write is rejected.

## The problem dibs solves

When two agents independently edit the same file:

1. Agent A reads `config.json` (contents hash to H0)
2. Agent B reads `config.json` (contents hash to H0)
3. Agent A writes a new version of `config.json`
4. Agent B writes a different version of `config.json`

Without dibs, step 4 silently overwrites A's work. With dibs, step 4 fails because dibs knows B's view of the file (H0) no longer matches the file's actual contents (which changed when A wrote in step 3).

## How conflict detection works

### Re-hash on write

dibs uses a simple principle: **at write time, re-hash the backing file and compare against the reader's hash**. There is no cached "current hash" that needs to be kept in sync. The backing file is always the source of truth.

### The CAS table

The core data structure is `CasTable` in `src/state/hash_table.rs`. It maintains two maps:

**`entries`**: `DashMap<PathBuf, Mutex<FileState>>` — one entry per tracked file, used only for write ownership tracking.

```rust
FileState {
    write_owner: Option<u64>,    // file handle that currently holds write permission
    last_access: DateTime<Utc>,  // for eviction
}
```

**`reader_hashes`**: `DashMap<(u32, PathBuf), ReaderEntry>` — one entry per (session, file) pair.

```rust
ReaderEntry {
    hash: Vec<u8>,               // hash the session last saw
    last_access: DateTime<Utc>,
}
```

The `reader_hashes` map records what each session *thinks* the file contains based on their last read. A conflict is detected when the reader's hash doesn't match the file's current hash (computed at write time).

### Session IDs, not PIDs

Agents often use subprocesses for file I/O. Claude Code, for example, spawns shell processes to run tools. If dibs tracked by PID, the reading process (PID 100) and writing process (PID 101) would look like different entities, and dibs couldn't connect B's read to B's write.

Instead, dibs uses the POSIX **session ID** (SID). All processes in the same terminal session share a SID. When a FUSE request arrives, dibs calls `getsid(pid)` to get the caller's session:

```rust
fn get_sid(pid: u32) -> u32 {
    let sid = unsafe { libc::getsid(pid as i32) };
    if sid < 0 { pid } else { sid as u32 }
}
```

This means "agent A" is really "all processes in terminal session A" — the shell, any subprocesses it spawns, editor plugins, etc. They all share one reader hash entry per file.

### The open/read/write/flush lifecycle

Here's what happens for a typical read-then-write cycle, annotated with what dibs does at each step.

**Reading a file** (`fs::read_to_string` → FUSE `open` with O_RDONLY):

```
open(O_RDONLY):
    hash = sha256(backing_file)
    reader_hashes[(sid, path)] = hash   // record what this session saw
    handle.hash_at_open = Some(hash)
```

The reader hash is the session's "receipt" — proof of what it saw.

**Writing a file** (`fs::write` → FUSE `open` with O_WRONLY|O_TRUNC, then `write`, then `flush`):

```
open(O_WRONLY):
    pre_hash = sha256(backing_file)      // hash BEFORE libc::open (which may truncate)
    fd = libc::open(path, O_WRONLY|...)  // this may truncate the file
    handle.hash_at_open = None           // write-only handle has no hash

    // CAS check at open time, using the pre-truncation hash:
    reader_hash = reader_hashes[(sid, path)]
    if reader_hash != pre_hash:
        close(fd); return EIO            // stale view → reject
    entries[path].write_owner = fh       // claim exclusive write

write(data):
    // write_owner already acquired at open → proceed directly
    pwrite(fd, data)

flush():
    new_hash = sha256(backing_file)      // hash the file after write
    reader_hashes[(sid, path)] = new_hash // update this session's receipt
    entries[path].write_owner = None      // release write lock
```

### Why the CAS check is at open time

Standard library calls like `fs::write` open the file with `O_WRONLY|O_TRUNC`, which truncates the file to zero bytes as a side effect of `open()`. If the CAS check happened later at `write()` time, the file would already be truncated — its hash would be the empty-file hash, not the pre-truncation content hash. Comparing the reader's hash against the empty-file hash would always fail, even for legitimate writes.

By checking at open time, before `libc::open` executes the truncation, the comparison is between the reader's hash and the file's actual pre-write content — exactly the right thing.

For writes that don't involve truncation (rare in practice — most tools use `O_CREAT|O_TRUNC`), there is a fallback CAS check in the `write()` handler that fires if no write ownership was established at open time.

### Unlink and rename CAS checks

When a file is deleted (`unlink`) or renamed, dibs checks if the calling session has a reader hash for the file. If so, it re-hashes the backing file and compares. If the file changed since the session last read it, the operation is rejected with `EIO`. If the session never read the file, the operation is allowed.

### O_RDWR handles

When a file is opened with O_RDWR (read and write simultaneously), the handle gets `hash_at_open = Some(hash)` at open time, just like a read. The CAS check uses this directly instead of looking up `reader_hashes`. This works because the hash was captured at open time, before any modifications.

### Blind writes

If a session writes to a file it never read (no entry in `reader_hashes` for that SID+path, and `hash_at_open` is None), dibs allows it. There's no prior read to conflict with. This handles cases like redirecting output to a new file.

## File hashing

`src/fs/cas.rs` computes content hashes:

- **Files ≤ 10 MB**: SHA-256 (32-byte output). Cryptographic strength, modest speed.
- **Files > 10 MB**: xxHash XXH3-128 (16-byte output). Much faster for large files, non-cryptographic but collision-resistant enough for change detection.

The threshold avoids spending seconds hashing large binary files on every open.

## Handle and inode tracking

**`HandleTable`** (`src/fs/handles.rs`): Maps FUSE file handles to their state — backing FD, path, hash at open, SID, write flag. Uses atomic counter for unique handle IDs.

**`InodeTable`** (`src/fs/inodes.rs`): Bidirectional map between inode numbers and relative paths. FUSE communicates in inodes; dibs needs paths for the backing filesystem and CAS table. Uses a reserved high range (`u64::MAX - 1000` and above) for the synthetic `.dibs/` virtual directory.

## Virtual `.dibs/` directory

The mount point contains a virtual `.dibs/` directory (not present in the backing filesystem) that exposes runtime state:

- `.dibs/status` — JSON with tracked file count, active write locks, uptime
- `.dibs/locks` — JSON array of all CAS entries with write owners
- `.dibs/conflicts/` — directory for saved rejected write data (if `--save-conflicts` is enabled)

These use synthetic inodes and are read-only.

## Eviction

The CAS table would grow without bound as files are opened. An eviction thread (`src/state/eviction.rs`) runs every 60 seconds and removes entries that haven't been accessed within the configured window (default: 60 minutes). Entries with active write owners are never evicted. Stale reader hash entries are cleaned up in the same pass.

The eviction thread sleeps in 1-second ticks rather than sleeping for the full 60-second interval. This ensures the shutdown flag is noticed within ~1 second — a previous implementation that slept for 60 seconds caused a 60-second hang on Ctrl-C.

## Startup and shutdown

### Startup (`src/main.rs`)

1. Validate backing directory and mountpoint
2. Check for stale FUSE mounts from previous crashes
3. Create `DibsFs` with all subsystems
4. Start eviction thread
5. Call `fuser::spawn_mount2()` to run FUSE in a background thread
6. Enter `wait_for_shutdown()` loop — polls a signal pipe (200ms timeout) and checks if the FUSE thread exited

### Shutdown

Two paths:

**Signal (Ctrl-C / SIGTERM)**: Signal handler writes to pipe → main thread wakes up → sets shutdown flag → joins eviction thread → calls `session.umount_and_join()` to force-unmount and join FUSE thread.

**External unmount** (`umount /mnt`): FUSE thread exits on its own → `guard.is_finished()` returns true in poll loop → main thread sets shutdown flag → joins eviction thread → calls `session.join()` (thread already exited).

Both paths complete within ~1 second due to the tick-based eviction sleep and 200ms poll timeout.

## Module map

```
src/
├── main.rs              signal handling, mount/unmount CLI, shutdown orchestration
├── lib.rs               re-exports modules
├── config.rs            CLI parsing (clap), DibsConfig struct
├── error.rs             DibsError enum (CasConflict, WriteOwnership, etc.)
├── fs/
│   ├── mod.rs           DibsFs struct, Filesystem trait impl (all FUSE operations)
│   ├── cas.rs           SHA-256 / xxHash file hashing
│   ├── handles.rs       HandleTable, HandleState (FH → fd/path/hash/sid)
│   ├── inodes.rs        InodeTable (inode ↔ path bidirectional map)
│   ├── passthrough.rs   libc wrappers (stat, fstat, lstat, path conversion)
│   └── virtual_dir.rs   .dibs/ directory name constants
└── state/
    ├── mod.rs
    ├── hash_table.rs    CasTable, FileState, ReaderEntry, conflict detection logic
    └── eviction.rs      background eviction thread
```
