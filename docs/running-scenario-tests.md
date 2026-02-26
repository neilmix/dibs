# Running Scenario Tests

The scenario tests in `tests/scenarios.rs` exercise all shutdown paths: ctrl-C, external unmount, busy mount, SIGKILL, signal races, etc. They spawn real `dibs mount` processes and need a working FUSE installation.

## Prerequisites

1. **FUSE library installed and working.** macFUSE, FUSE-T, or libfuse — whichever you use for normal `dibs mount`.

2. **Permission to mount.** On macOS this usually works without sudo. On Linux you may need membership in the `fuse` group or root.

3. **No sandbox.** The tests spawn child processes, create FUSE mounts, send signals, and call `umount`. A sandbox that restricts any of these will cause failures.

## Running with Claude Code

Claude Code runs in a sandbox by default that blocks the syscalls these tests need. To run them interactively:

1. Disable the sandbox (the tests need real FUSE mounts and signals).
2. Enable permission checks so Claude asks before running each command — you can review each `cargo test` invocation before it executes.

Then ask Claude to run:

```
cargo test --test scenarios -- --ignored --test-threads=1
```

Or run a single scenario:

```
cargo test --test scenarios scenario_01 -- --ignored
```

## Running manually

```
cargo test --test scenarios -- --ignored --test-threads=1
```

`--test-threads=1` is recommended to avoid overwhelming the FUSE subsystem. It is not strictly required — each test uses unique temp directories — but serial execution makes failures easier to diagnose.

## What the tests do

Each test:

1. Creates temp directories for backing store, mountpoint, and log file.
2. Spawns `dibs mount` as a child process.
3. Waits for the mount to appear (polls `mount` command output).
4. Performs the scenario action (send signal, run `dibs unmount`, hold mount busy, etc.).
5. Asserts on exit code, timing, stderr messages, and mount state.
6. Cleans up via a `Drop` impl that SIGKILLs the child and force-unmounts if needed.

## Interpreting failures

**"dibs mount did not appear within 5s"** — FUSE isn't working. Check that your FUSE library is installed and that you can run `dibs mount` manually.

**Timing assertions fail (> 1 second)** — The shutdown path is blocking somewhere. Common causes: `umount` hanging on a busy mount, eviction thread not responding to the shutdown flag, or `BackgroundSession::drop` waiting for the FUSE thread.

**"expected 'busy' in stderr"** — The `umount` command didn't report "busy" when a process had its cwd in the mount. This may vary by OS or FUSE implementation. Check what `umount` actually prints on your system.

**"dibs mount process did not exit after external unmount"** — The `wait_for_shutdown` poll loop isn't detecting `guard.is_finished()`. Check that the 200ms poll/is_finished loop in `main.rs` is working correctly.

**Scenario 10 (SIGKILL) mount not cleaned up** — macFUSE does NOT auto-cleanup stale mounts after the FUSE process exits. The mount entry lingers in `mount` output even though access fails with "Device not configured". The tests account for this — they assert the mount is non-functional rather than absent, and the `Drop` impl force-unmounts via `umount -f`.

## Scenario reference

See `SCENARIOS.md` in the project root for the full specification of expected behavior in each scenario.
