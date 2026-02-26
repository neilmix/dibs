# dibs

A FUSE filesystem that prevents multiple AI coding agents from stomping on each other's work.

## The problem

You're running two AI coding sessions on the same project to stay productive while waiting on inference. Agent A is refactoring the auth module. Agent B is building a new dashboard component. They're both fast, they're both confident, and they both just edited `utils/api.ts` at the same time. Agent B's changes silently overwrote Agent A's. Nobody noticed until the build broke twenty minutes later.

You could use git worktrees or separate checkouts, but then you're dealing with merge conflicts, duplicate servers fighting over ports, and two copies of `node_modules`. You just want both agents working in the same directory without silently clobbering each other.

## How dibs works

dibs mounts a thin FUSE layer over your project directory. Reads work normally. Writes use optimistic concurrency control: every time a file is read, dibs records a hash of its contents. When a write comes in, dibs checks whether the file has changed since the writer last read it. If it has, the write is rejected.

```
Agent A reads utils/api.ts        → dibs records hash abc123
Agent B reads utils/api.ts        → dibs records hash abc123
Agent A writes utils/api.ts       → hash still abc123, write succeeds, new hash def456
Agent B writes utils/api.ts       → hash is now def456, doesn't match abc123, write REJECTED
```

Agent B gets an I/O error. It has to re-read the file and decide what to do. The important thing is that Agent A's work is never silently lost.

This works especially well with AI coding tools because they naturally read a file, spend a while thinking, then write the whole file back — which maps perfectly to the read-hash-write cycle.

## Quick start

### Prerequisites

A FUSE library must be installed on the host:
- **macOS**: [macFUSE](https://osxfuse.github.io/) or [FUSE-T](https://www.fuse-t.org/)
- **Linux**: `libfuse` (e.g. `apt install libfuse-dev` or `dnf install fuse-devel`)

### Build

```bash
cargo build
```

### Set up permissions

The backing directory should be non-writable by your agents so they can't bypass dibs. How you do this depends on your OS and setup — standard filesystem permissions or ACLs both work. The key idea: agents read and write through the dibs mount point, not the backing directory directly.

On macOS, [sandbash](https://github.com/neilmix/sandbash) is a good option. It sandboxes a command so it can only write to designated directories. Point it at the mount point and the agent can't touch the backing directory:

```bash
sandbash -w /path/to/mountpoint -- your-agent-command
```

### Mount

```bash
# Mount your project through dibs
dibs mount /path/to/project /path/to/mountpoint

# Point your agents at the mount point
cd /path/to/mountpoint
```

### Run your agents

Start your coding agents targeting the mount point. They'll read and write files normally. dibs handles the rest.

### Unmount

```bash
dibs unmount /path/to/mountpoint
```

## Mount options

```bash
dibs mount /path/to/backing /path/to/mount \
  --session-id "agent-a"      \  # Label for log entries (default: dibs-<pid>)
  --log-file /tmp/dibs.log    \  # Log file location (default: /tmp/dibs.log)
  --eviction-minutes 60       \  # Evict unused hash entries after N minutes (default: 60)
  --save-conflicts               # Save rejected writes for recovery (default: off)
```

When `--save-conflicts` is enabled, rejected write data is saved to a `.dibs-conflicts/` directory inside the backing directory, with filenames like `20250226_143200_123_api.ts` (timestamp + original filename). This lets you manually recover rejected content.

## Watching for conflicts

dibs exposes a virtual `.dibs/` directory at the mount root (it doesn't exist in your backing directory).

**Check what files are currently tracked:**

```bash
cat /path/to/mountpoint/.dibs/locks
```

Returns JSON:

```json
[
  {
    "path": "src/auth.ts",
    "hash": "a1b2c3...",
    "write_owner": null,
    "last_access": "2025-02-26T14:30:00Z"
  },
  {
    "path": "src/api.ts",
    "hash": "d4e5f6...",
    "write_owner": 42,
    "last_access": "2025-02-26T14:31:22Z"
  }
]
```

A non-null `write_owner` means a file handle currently has write ownership of that file.

**Check daemon status:**

```bash
cat /path/to/mountpoint/.dibs/status
```

Returns JSON:

```json
{
  "tracked_files": 12,
  "active_locks": 1,
  "uptime_seconds": 3600,
  "session_id": "agent-a"
}
```

## How agents experience conflicts

When a write is rejected, the agent sees a write failure (EIO). What happens next depends on the agent:

- **Claude Code**: Reports the write failed. You can tell it to re-read the file and adapt.
- **Aider**: Will typically notice the error and ask what to do.
- **Cursor / Copilot**: Behavior varies.

## Configuring your agent

If you're using Claude Code, add something like the following to your project's `CLAUDE.md`. Adapt as needed for other agents' custom instruction mechanisms.

```markdown
## Concurrent editing (dibs)

This project uses dibs for optimistic file-level concurrency control. Multiple agents work here simultaneously.

- If a write fails (I/O error), another agent changed the file since you last read it. Your write was NOT applied.
- On failure: re-read the file, reconcile your changes with the new content, then retry.
- NEVER retry a failed write without re-reading first — it will fail again.
- Check `.dibs/locks` before starting to avoid files other agents are actively editing.
- If you hit repeated write failures on the same file, tell the user.
```

Users working with other agents (Aider, Cline, etc.) can adapt this language for their respective configuration files or system prompts.

## Limitations

- **No merging.** dibs doesn't try to merge concurrent edits. The second writer loses. This is by design — silent merges are worse than loud failures.
- **Per-file granularity.** If two agents edit different functions in the same file, dibs still rejects the second write. Consider breaking large files into smaller modules.
- **Single machine only.** dibs is not a distributed filesystem.
- **Some overhead on write.** Each write re-reads and hashes the backing file to verify. Negligible for source code, potentially noticeable for very large files.

## License

MIT
