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

### The CAS table

The core data structure is `CasTable` in `src/state/hash_table.rs`. It maintains two maps:

**`entries`**: `DashMap<PathBuf, Mutex<FileState>>` — one entry per tracked file.

```rust
FileState {
    hash: Option<Vec<u8>>,       // current content hash (SHA-256 or xxHash)
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

The `entries` map records what the file actually contains. The `reader_hashes` map records what each session *thinks* the file contains based on their last read. A conflict occurs when these disagree.

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
    cas_table.entries[path].hash = hash          // update "truth"
    cas_table.reader_hashes[(sid, path)] = hash  // record what this session saw
    handle.hash_at_open = Some(hash)
```

The reader hash is the session's "receipt" — proof of what it saw.

**Writing a file** (`fs::write` → FUSE `open` with O_WRONLY, then `write`, then `flush`):

```
open(O_WRONLY):
    cas_table.record_write_open(path)  // ensure entry exists, but DON'T update hash
    handle.hash_at_open = None         // write-only handle has no hash

write(data):
    check_and_acquire_write(path, fh, sid):
        reader_hash = reader_hashes[(sid, path)]   // what did this session last read?
        current_hash = entries[path].hash           // what does the file actually contain?
        if reader_hash != current_hash:
            return Err("CAS conflict")              // stale view → reject
        entries[path].write_owner = fh              // claim exclusive write

flush():
    new_hash = sha256(backing_file)                 // hash the file after write
    entries[path].hash = new_hash                   // update truth
    reader_hashes[(sid, path)] = new_hash           // update this session's receipt
    entries[path].write_owner = None                // release write lock
```

The key insight: `record_write_open` does NOT update the CAS hash. This is what makes conflict detection work. If it updated the hash, every write-mode open would refresh the "truth" to match the file's current state, and the subsequent comparison would always succeed — which is exactly the bug that existed before.

### Why write-only opens don't update the hash

Consider the conflict scenario:

```
A reads  → entries[f].hash = H0, reader_hashes[(A, f)] = H0
B reads  → entries[f].hash = H0, reader_hashes[(B, f)] = H0
A writes → reader_hashes[(A, f)] = H0 == entries[f].hash = H0 → OK
           flush: entries[f].hash = H1, reader_hashes[(A, f)] = H1
B writes → reader_hashes[(B, f)] = H0 != entries[f].hash = H1 → CONFLICT
```

If B's `open(O_WRONLY)` had updated `entries[f].hash` by re-reading the file, it would have set it to H1 (A's version). Then B's reader hash (H0) would fail against H1 — which is correct, the conflict is still detected. But what if the open updated the reader hash too? Then it would be `reader_hashes[(B, f)] = H1`, and the check would pass — silently losing A's work. The separation between "update on read" and "don't update on write" is what makes the whole scheme work.

### O_RDWR handles

When a file is opened with O_RDWR (read and write simultaneously), the handle gets `hash_at_open = Some(hash)` at open time, just like a read. The CAS check uses this directly instead of looking up `reader_hashes`. This works because the hash was captured at open time, before any modifications.

### Blind writes

If a session writes to a file it never read (no entry in `reader_hashes` for that SID+path, and `hash_at_open` is None), dibs allows it. There's no prior read to conflict with. This handles cases like redirecting output to a new file.

## The file watcher

dibs also needs to detect changes that bypass FUSE entirely — direct edits to the backing directory, `git checkout`, etc. A filesystem watcher (`notify` crate, using FSEvents on macOS / inotify on Linux) monitors the backing directory and invalidates CAS entries when it sees changes.

When the watcher detects a modification, it sets the file's hash to a sentinel value (`[0xff; 32]`) that won't match any real hash. The next write through FUSE will fail the CAS check.

### Self-write suppression

The problem: dibs's own writes to the backing directory also trigger watcher events. Without suppression, every FUSE write would invalidate its own CAS entry.

This is handled by three layers of suppression in `src/watcher/mod.rs`:

**Layer 1: `expected_writes`** (`DashSet<PathBuf>`)

Before writing to the backing file, dibs inserts the path into this set. When the watcher fires, it checks `expected_writes.remove(path)` — if it was there, the event is suppressed.

**Layer 2: `has_active_writer`**

A single FUSE write can generate multiple filesystem events (e.g., CREATE + MODIFY for `O_CREAT|O_TRUNC`). Layer 1 only catches the first because the set contains one entry. Layer 2 checks if the file still has an active write owner — if so, the event is from our ongoing write.

**Layer 3: `recent_self_writes`** (`DashMap<PathBuf, Instant>`)

macOS FSEvents can deliver events with 100ms–1s of latency. By the time the event arrives, flush may have already released write ownership and removed the expected_writes entry. Layer 3 records a timestamp when each file is flushed and suppresses events within 2 seconds.

The complete flow in the watcher:

```
event arrives for path:
    if expected_writes.remove(path)     → suppress (Layer 1)
    if has_active_writer(path)          → suppress (Layer 2)
    if recent_self_writes[path] < 2s    → suppress (Layer 3)
    else                                → invalidate CAS entry
```

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
- `.dibs/locks` — JSON array of all CAS entries with hashes and write owners
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
├── state/
│   ├── mod.rs
│   ├── hash_table.rs    CasTable, FileState, ReaderEntry, conflict detection logic
│   └── eviction.rs      background eviction thread
└── watcher/
    └── mod.rs           filesystem watcher, three-layer self-write suppression
```
