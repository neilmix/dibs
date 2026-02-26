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

## Watcher self-write suppression

Two-layer guard in `src/watcher/mod.rs`. A single FUSE write inserts one entry in `expected_writes`, but the OS may emit multiple FS events (e.g. CREATE + MODIFY for `O_CREAT|O_TRUNC`). Layer 1: `expected_writes.remove()` catches the first event. Layer 2: `cas_table.has_active_writer()` catches subsequent events while the handle still holds write ownership. Both layers must stay in sync — removing either re-introduces the CAS false-positive bug.

## macFUSE `AutoUnmount` caveat

macFUSE does **not** remove stale mount entries after the FUSE process exits. The mount stays in `mount` output (access returns "Device not configured"). Only `umount -f` clears it. Tests must not assert on automatic mount cleanup — use force-unmount in cleanup instead.
