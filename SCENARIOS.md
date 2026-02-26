# Shutdown Scenarios

How `dibs` should behave across all combinations of mount, busy state, and shutdown method.

## Setup

- **Shell 1**: runs `dibs mount <backing> <mountpoint>`
- **Shell 2**: may `cd` into `<mountpoint>` (making it busy) or not
- **Shell 3**: may run `dibs unmount <mountpoint>`

## Scenarios

### 1. Ctrl-C, not busy

Shell 2 is not using the mountpoint.
Shell 1 presses ctrl-C.

| Shell | Sees | Timing |
|-------|------|--------|
| 1 | `dibs: unmounting (received signal)...` then `dibs: unmounted <mountpoint>` | < 1 second, process exits 0 |

### 2. Ctrl-C, busy

Shell 2 has `cd`'d into the mountpoint.
Shell 1 presses ctrl-C.

| Shell | Sees | Timing |
|-------|------|--------|
| 1 | `dibs: unmounting (received signal)...` then `dibs: unmounted <mountpoint>` | < 1 second, process exits 0 |
| 2 | Subsequent commands in the mount (ls, cat, etc.) fail with I/O errors or "Transport endpoint is not connected" | Immediate |

Note: The kernel allows FUSE unmount even when busy — the mount goes away and in-flight operations get errors. This is unlike block device mounts. `AutoUnmount` reinforces this: the mount is tied to the FUSE process. When the process exits, the mount disappears regardless of who's using it.

Ctrl-C should always work promptly. The mount being busy is not the FUSE daemon's problem — it's the users of the mount who will see errors.

### 3. External unmount (shell 3), not busy

Shell 2 is not using the mountpoint.
Shell 3 runs `dibs unmount <mountpoint>`.

| Shell | Sees | Timing |
|-------|------|--------|
| 3 | `Unmounting <mountpoint>...` then `Successfully unmounted <mountpoint>` | < 1 second, exits 0 |
| 1 | `dibs: unmounted <mountpoint>`, process exits 0 | < 1 second after shell 3 completes |

### 4. External unmount (shell 3), busy

Shell 2 has `cd`'d into the mountpoint.
Shell 3 runs `dibs unmount <mountpoint>`.

| Shell | Sees | Timing |
|-------|------|--------|
| 3 | `Unmounting <mountpoint>...` then `Mount point is busy. Make sure no shells or processes are using <mountpoint>, then try again.` | < 1 second, exits 1 |
| 1 | Nothing new — dibs keeps running | N/A |
| 2 | Unaffected, mount still works | N/A |

The unmount is refused. The dibs process does not find out and does not need to — nothing happened to its FUSE session.

### 5. External unmount (shell 3), busy, then not busy, then retry

Shell 2 has `cd`'d into the mountpoint.
Shell 3 runs `dibs unmount <mountpoint>` — fails with busy message.
Shell 2 runs `cd /` (leaves the mountpoint).
Shell 3 runs `dibs unmount <mountpoint>` again.

| Shell | Sees | Timing |
|-------|------|--------|
| 3 (1st) | `Mount point is busy...` | < 1 second, exits 1 |
| 3 (2nd) | `Successfully unmounted <mountpoint>` | < 1 second, exits 0 |
| 1 | `dibs: unmounted <mountpoint>`, process exits 0 | < 1 second after shell 3's second attempt |

### 6. External unmount (shell 3) then ctrl-C in shell 1

Shell 3 successfully unmounts.
Shell 1 hasn't exited yet (within the detection window).
User presses ctrl-C in shell 1.

| Shell | Sees | Timing |
|-------|------|--------|
| 1 | `dibs: unmounted <mountpoint>`, exits 0 | Whichever triggers first — the external unmount detection or the signal. Either way, clean exit within 1 second. |

No double-unmount error should be visible to the user. If the FUSE session is already gone, the signal path should handle the "already unmounted" case gracefully.

### 7. Ctrl-C during startup (mount not yet ready)

Shell 1 presses ctrl-C while FUSE session is still initializing.

| Shell | Sees | Timing |
|-------|------|--------|
| 1 | Process exits | < 1 second |

If the session hasn't been spawned yet, normal process termination. If it has been spawned but the signal pipe isn't set up yet, the default SIGINT behavior (kill) should still apply. The process should never hang during startup.

### 8. Multiple ctrl-C

Shell 1 presses ctrl-C. User gets impatient and presses ctrl-C again.

| Shell | Sees | Timing |
|-------|------|--------|
| 1 | Same as scenario 1 or 2. Second ctrl-C is harmless. | < 1 second total |

The second signal should not cause a crash, double-free, or hang. If unmount is already in progress, extra signals are ignored or idempotent.

### 9. SIGTERM (e.g. `kill <pid>`)

Same as ctrl-C (scenarios 1 and 2) but triggered via SIGTERM instead of SIGINT.

| Shell | Sees | Timing |
|-------|------|--------|
| 1 | Same output as ctrl-C path | < 1 second |

### 10. SIGKILL (e.g. `kill -9 <pid>`)

Cannot be caught. The process dies immediately.

| Shell | Sees | Timing |
|-------|------|--------|
| 1 | Killed | Immediate |
| 2 | I/O errors on subsequent mount access | Immediate |

With `AutoUnmount`, the kernel *may* clean up the mount point when the FUSE process disappears. In practice, macFUSE does **not** auto-remove stale mounts — the entry stays in `mount` output (access returns "Device not configured") until cleared with `umount -f`. This varies by FUSE implementation.

## Invariants

Across all scenarios, these should always hold:

1. **No hang on ctrl-C.** The process exits within 1 second of receiving SIGINT or SIGTERM.
2. **No panic on shutdown.** The eviction thread, FUSE thread, and main thread all shut down without UB or panic.
3. **No zombie mounts.** After the dibs process exits, the mountpoint is no longer functional. Note: macFUSE may leave a stale entry in `mount` output that requires `umount -f` to clear.
4. **Busy = informative refusal.** When `dibs unmount` fails because the mount is busy, the user is told what to do — not shown a generic error or told to `sudo umount -f`.
5. **Clean exit code.** 0 on success, 1 on failure. Ctrl-C exit is 0 (intentional shutdown).
6. **Idempotent unmount.** Running `dibs unmount` on an already-unmounted path doesn't crash — it reports the path isn't mounted.
