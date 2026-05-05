[← Manual](../index.md)

# braid recover

Resumes from an interrupted operation (add, remove, replace) by opening LUKS devices, mounting the pool, rebuilding `pool.json` from live pool state when appropriate, finishing owed maintenance, and clearing the pending-operation journal.

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
2. Chooses the mount membership from the journal phase. Add and remove-missing `PoolMutation` phases mount from the pre-operation membership. Add, remove-missing, and replace post-maintenance phases mount from the committed target membership. Replace `PoolMutation` uses the pre/target union because the kernel may still be completing `dev_replace`.
3. Opens LUKS devices and mounts the pool (or reuses the existing mount if already mounted). **Exception:** if the journal records `Replace::PoolMutation` and the pool is already mounted by an external process, recover refuses -- run `sudo braid lock` followed by `sudo braid recover` so a fresh mount session can clear any kernel-resumed-dev_replace staleness via the relock cycle. Replace post-maintenance recovery is allowed on an already-mounted pool.
4. For `Replace::PoolMutation` only, if a kernel-resumed btrfs replace is in progress, waits for it to finish.
5. For `Replace::PoolMutation` only, if the pool was just mounted by this recover run, performs a full relock-and-remount cycle (umount, `btrfs device scan --forget`, close LUKS, reopen, remount) to ensure the kernel's in-memory device topology matches the on-disk state.
6. Probes the live pool to discover actual membership.
7. For add `PoolMutation`, replays only journaled targets that are not already live. Fresh targets use the stored LUKS format options from the original add. Verified returned braid-labeled targets may use `btrfs device add -f` after `wipefs --all --types btrfs`; other targets are not force-added.
8. For add `PostAddBalanceRaid1`, does not format, enroll, back up headers as target prep, wipe, or add disks. It only validates the committed live pool and finishes the owed RAID1 balance.
9. For replace and remove-missing `PoolMutation`, detects whether the primary btrfs membership mutation committed. If it did not commit, recover restores/keeps the pre-operation `pool.json`, clears the journal, and tells you to rerun the original command. It does not rerun `btrfs replace start` or `btrfs device remove`.
10. For replace and remove-missing post-maintenance phases, validates committed live membership, repairs `pool.json` if needed, and finishes only owed maintenance such as resize, paused-balance resume, or soft RAID1 balance.
11. Resolves `/dev/disk/by-id/` paths from the live device identity (not from the journal, which may be stale).
12. Writes or repairs `pool.json` only after the journal phase allows it and live membership is complete.
13. Clears `pending-op.json` only after membership is complete and any owed balance work is done.

## Safety checks

- Refuses if no `pending-op.json` exists.
- Refuses to adopt live pool members that aren't in either the pre-operation or target journal snapshot (guards against devices added outside braid).
- Hard-fails if a live pool device has no `/dev/disk/by-id/` symlink (recovery can't guess a stable identifier).
- Detects interrupted bootstrap add (first disk, no filesystem yet) and gives specific wipe-and-retry instructions instead of a confusing mount error.
- For existing-pool add recovery, refuses to clear the journal while any journaled add target is missing from the live pool.
- Once an add journal reaches `PostAddBalanceRaid1`, refuses to replay disk preparation or btrfs membership mutation.
- Once replace or remove-missing reaches its post-maintenance phase, refuses to rerun the primary btrfs membership mutation.
- Without `--allow-degraded`, refuses to mount if devices are missing (exit code 2 for degraded-refused, distinguishing it from other errors).
- Refuses to recover `Replace::PoolMutation` when the pool is already mounted (admin-mounted, circumventing braid's pending-op preflight on `unlock`). The kernel may have resumed an interrupted `dev_replace` on that mount session, leaving stale in-memory device state that recover cannot scrub without unmounting -- which it will not do on a mount it does not own. Remediation: `sudo braid lock; sudo braid recover`.

## Related commands

- [status](status.md) -- shows pending operation state and prompts you to recover
- [discover](discover.md) -- rebuild pool.json from LUKS labels (when there's no journal)
- [unlock](unlock.md) -- normal unlock (when no journal exists)

## Related guides

- [Recovery scenarios](../guides/recovery-scenarios.md)
