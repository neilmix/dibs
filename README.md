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

### Build

```bash
cargo build --release
```

### Set up permissions

The backing directory needs to be non-writable by your agents so they can't bypass dibs:

```bash
# Create a dedicated user for dibs
sudo useradd -r -s /bin/false dibs

# Change ownership of your project's backing copy
sudo chown -R dibs:dibs /home/you/myproject

# Allow your user to read but not write
sudo chmod -R o+rX,o-w /home/you/myproject
```

### Mount

```bash
# Mount your project through dibs
dibs mount /home/you/myproject /mnt/myproject

# Point your agents at the mount point
cd /mnt/myproject
```

### Run your agents

Start your coding agents targeting `/mnt/myproject`. They'll read and write files normally. dibs handles the rest.

### Unmount

```bash
dibs unmount /mnt/myproject
# or
fusermount -u /mnt/myproject
```

## Mount options

```bash
dibs mount /path/to/backing /path/to/mount \
  --session-id "agent-a"      \  # Label for log entries (default: none)
  --log-file /tmp/dibs.log    \  # Conflict log location (default: /tmp/dibs.log)
  --eviction-minutes 60       \  # Evict unused hash entries after N minutes (default: 60)
  --save-conflicts             \  # Save rejected writes for recovery (default: on)
  --readonly-fallback          \  # On conflict, silently make fd read-only instead of EIO
```

### A note on `--readonly-fallback`

By default, dibs returns an I/O error (`EIO`) when a write is rejected. Most tools handle this fine. Some don't — they might retry in a loop or crash. If you're hitting that, `--readonly-fallback` silently drops the write instead. It's less correct but more compatible. Try without it first.

## Watching for conflicts

dibs exposes a virtual `.dibs/` directory at the mount root (it doesn't exist in your backing directory).

**Check what files are currently tracked:**

```bash
cat /mnt/myproject/.dibs/locks
```

Returns JSON:

```json
{
  "files": {
    "src/auth.ts": { "hash": "a1b2c3...", "last_read": "2025-02-26T14:30:00Z" },
    "src/api.ts": { "hash": "d4e5f6...", "last_read": "2025-02-26T14:31:22Z" }
  }
}
```

**Check daemon status:**

```bash
cat /mnt/myproject/.dibs/status
```

**Review rejected writes:**

```bash
ls /mnt/myproject/.dibs/conflicts/
# src_api.ts.2025-02-26T14:32:00Z
```

These are the contents that were rejected, so you can manually recover them if needed.

## How agents experience conflicts

When a write is rejected, the agent sees a write failure (EIO or a silent no-op depending on your settings). What happens next depends on the agent:

- **Claude Code**: Reports the write failed. You can tell it to re-read the file and adapt.
- **Aider**: Will typically notice the error and ask what to do.
- **Cursor / Copilot**: Behavior varies. Test with `--readonly-fallback` if you see issues.

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
