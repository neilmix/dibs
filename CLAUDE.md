# dibs

FUSE filesystem enforcing optimistic concurrency control for multi-agent environments.

## Build

Requires a FUSE library installed on the host (macFUSE, FUSE-T, libfuse, etc.). Standard `cargo build`.

Uses fuser 0.17 for FUSE bindings (`spawn_mount2` for background sessions).

## Conventions

- **Multi-platform**: no platform-specific code or config. Do not add build workarounds tied to a single OS or architecture.

## Shutdown ordering

The eviction thread holds a raw pointer to `CasTable` inside `DibsFs`. On shutdown, the eviction thread **must** be stopped before the FUSE session is joined/dropped — joining the session drops `DibsFs` and frees the `CasTable`. Reversing this order causes UB (the panic reported with `slice::from_raw_parts`). The real fix is `Arc<CasTable>` — tracked by the TODO in `main.rs`.

The eviction thread sleeps in 1-second ticks (not the full check interval) so it notices the shutdown flag within ~1 second. A previous 60-second sleep was the root cause of the ctrl-C hang bug.

## Known issue: eviction pointer UB

The eviction thread's raw `cas_ptr` is technically dangling from the moment `spawn_mount2` moves `DibsFs` into the FUSE thread. It works in practice because the FUSE thread keeps the allocation alive, but it is UB. The fix is wrapping `CasTable` in an `Arc`. See the TODO comment in `src/main.rs`.

## Scenario tests

`tests/scenarios.rs` — 10 integration tests covering all shutdown paths (ctrl-C, external unmount, busy mount, SIGKILL, etc.). Requires real FUSE; ignored by default.

```
cargo test --test scenarios -- --ignored --test-threads=1
```

See `docs/running-scenario-tests.md` for full setup instructions.

## macFUSE `AutoUnmount` caveat

macFUSE does **not** remove stale mount entries after the FUSE process exits. The mount stays in `mount` output (access returns "Device not configured"). Only `umount -f` clears it. Tests must not assert on automatic mount cleanup — use force-unmount in cleanup instead.
