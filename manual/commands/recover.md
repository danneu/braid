[← Manual](../index.md)

# braid recover

Resumes from an interrupted operation (add, remove, replace) by opening LUKS devices, mounting the pool, rebuilding `pool.json` from live pool state, and clearing the pending-operation journal.

## When to use it

- After a crash, power failure, or interrupted braid command.
- When `braid status` or other commands show "pending operation -- run `braid recover`".
- Only available when `pending-op.json` exists.

## Basic example

```
sudo braid recover
```

You'll be prompted for the pool passphrase. Output shows the recovery process:

```
Recovering from interrupted Add operation (started 2026-03-15T14:30:00Z)...
  pre-operation membership:  {"ironwolf", "toshiba"}
  target membership:         {"ironwolf", "toshiba", "wdc"}
  recovered (live pool):     {"ironwolf", "toshiba", "wdc"}
note: target membership achieved — the interrupted operation completed before the crash.
pool.json written from live pool state.
pending-op.json cleared. Recovery complete.
```

## Common variations

Non-interactive (passphrase from stdin):

```
echo -n 'my-passphrase' | sudo braid recover --passphrase-stdin
```

Passphrase from a file:

```
sudo braid recover --passphrase-file /root/passphrase.txt
```

Recover with a missing disk (degraded mode):

```
sudo braid recover --allow-degraded
```

Preview what would happen:

```
sudo braid recover --dry-run
```

## Flags

| Flag | Effect |
| --- | --- |
| `--passphrase-stdin` | Read passphrase from stdin instead of TTY prompt |
| `--passphrase-file <path>` | Read passphrase from a file instead of TTY prompt |
| `--allow-degraded` | Allow mounting with missing devices (new writes have no redundancy) |
| `--dry-run` | Show what would be done without making changes |

## What happens under the hood

1. Loads `pending-op.json` (refuses if absent -- nothing to recover).
2. Computes a union membership from both the pre-operation and target snapshots in the journal, ensuring every device referenced by the interrupted operation can be opened.
3. Opens LUKS devices and mounts the pool (or reuses the existing mount if already mounted). **Exception:** if the journal records a `replace` operation and the pool is already mounted by an external process, recover refuses -- run `sudo braid lock` followed by `sudo braid recover` so a fresh mount session can clear any kernel-resumed-dev_replace staleness via the relock cycle.
4. If a kernel-resumed btrfs replace is in progress (from a crash during `braid replace`), waits for it to finish.
5. If the pool was just mounted by this recover run, performs a full relock-and-remount cycle (umount, `btrfs device scan --forget`, close LUKS, reopen, remount) to ensure the kernel's in-memory device topology matches the on-disk state.
6. Probes the live pool to discover actual membership.
7. Resolves `/dev/disk/by-id/` paths from the live device identity (not from the journal, which may be stale).
8. Writes the recovered membership to `pool.json`.
9. Clears `pending-op.json`.
10. Warns if a paused btrfs balance is detected.

## Safety checks

- Refuses if no `pending-op.json` exists.
- Refuses to adopt live pool members that aren't in either the pre-operation or target journal snapshot (guards against devices added outside braid).
- Hard-fails if a live pool device has no `/dev/disk/by-id/` symlink (recovery can't guess a stable identifier).
- Detects interrupted bootstrap add (first disk, no filesystem yet) and gives specific wipe-and-retry instructions instead of a confusing mount error.
- Without `--allow-degraded`, refuses to mount if devices are missing (exit code 2 for degraded-refused, distinguishing it from other errors).
- Refuses to recover a `replace` operation when the pool is already mounted (admin-mounted, circumventing braid's pending-op preflight on `unlock`). The kernel may have resumed an interrupted `dev_replace` on that mount session, leaving stale in-memory device state that recover cannot scrub without unmounting -- which it will not do on a mount it does not own. Remediation: `sudo braid lock; sudo braid recover`.

## Related commands

- [status](status.md) -- shows pending operation state and prompts you to recover
- [discover](discover.md) -- rebuild pool.json from LUKS labels (when there's no journal)
- [unlock](unlock.md) -- normal unlock (when no journal exists)

## Related guides

- [Recovery scenarios](../guides/recovery-scenarios.md)
