# dibs

FUSE filesystem enforcing optimistic concurrency control for multi-agent environments.

## Build

Requires a FUSE library installed on the host (macFUSE, FUSE-T, libfuse, etc.). Standard `cargo build`.

Uses fuser 0.17 for FUSE bindings (`spawn_mount2` for background sessions).

## Conventions

- **Multi-platform**: no platform-specific code or config. Do not add build workarounds tied to a single OS or architecture.

## Eviction thread

The eviction thread sleeps in 1-second ticks (not the full check interval) so it notices the shutdown flag within ~1 second. A previous 60-second sleep was the root cause of the ctrl-C hang bug.

## Scenario tests

`tests/scenarios.rs` — 10 integration tests covering all shutdown paths (ctrl-C, external unmount, busy mount, SIGKILL, etc.). Requires real FUSE; ignored by default.

```
cargo test --test scenarios -- --ignored --test-threads=1
```

See `docs/running-scenario-tests.md` for full setup instructions.

## CAS conflict detection

CAS checks re-hash the backing file and compare against the reader's hash. No filesystem watcher or hash cache is needed. External modifications (direct edits to the backing directory, `git checkout`, etc.) are detected immediately because the backing file is always the source of truth.

For write-mode opens, the CAS check happens at open time BEFORE `libc::open` (which may truncate the file). The pre-truncation hash is compared against the session's reader hash. O_WRONLY handles must have `hash_at_open = None` so the CAS logic uses `reader_hashes`, not the handle hash.

## macFUSE `AutoUnmount` caveat

macFUSE does **not** remove stale mount entries after the FUSE process exits. The mount stays in `mount` output (access returns "Device not configured"). Only `umount -f` clears it. Tests must not assert on automatic mount cleanup — use force-unmount in cleanup instead.
