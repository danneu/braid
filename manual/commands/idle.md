[← Manual](../index.md)

# braid idle

Check if the pool has any active operations. Designed for autosuspend integration.

## When to use it

- As an autosuspend check to prevent the system from sleeping during a scrub or any btrfs exclusive operation (balance, device add, device remove, device replace, resize, swap activate)
- In scripts that need to wait for the pool to be idle before proceeding

## Basic example

```
sudo braid idle
```

Output when the pool is mounted and idle:

```
idle: pool is idle
```

Output when the pool is not mounted (still exit 0 -- nothing to protect):

```
idle: pool is offline
```

## Exit codes

| Exit code | Meaning |
|---|---|
| **0** | Pool is idle (no operations running), OR pool is offline |
| **1** | Pool is busy (a scrub or btrfs exclusive operation is running) |
| **2** | Error (could not determine pool state) |

The busy reason is printed to stdout:

```
busy: scrub running (45%)
busy: balance running
busy: balance paused
busy: device add in progress
busy: device remove in progress
busy: device replace in progress
busy: resize in progress
busy: swap activate in progress
```

Only the scrub line carries a percentage. The other states come from
`/sys/fs/btrfs/<fsid>/exclusive_operation`, which reports the active
operation but not its progress.

When the pool is offline (not mounted), exit code is 0 -- there is nothing to protect, so suspend is safe.

## Autosuspend integration

braid idle is designed to be used as an [autosuspend](https://autosuspend.readthedocs.io/) check. Add it to your autosuspend configuration:

```ini
[check.BraidIdle]
class = CommandMixin
command = braid idle
```

The fail-closed design means that if the check itself errors (exit 2), autosuspend treats it as "activity detected" and blocks suspend. This is the safe default: if we cannot determine whether an operation is running, we must not allow suspend.

## What happens under the hood

1. Checks if the pool is mounted (via `findmnt`)
2. If not mounted: returns idle (exit 0)
3. Checks scrub status via `btrfs scrub status` (scrub is not in the kernel exclusive-operation set, so sysfs cannot detect it)
4. Reads `/sys/fs/btrfs/<fsid>/exclusive_operation` for any other active exclusive operation: `balance`, `balance paused`, `device add`, `device remove`, `device replace`, `resize`, `swap activate`
5. Returns busy on the first active operation found (short-circuits -- the scrub probe runs first so the common scrub-in-progress case skips the sysfs read)

## Related commands

- [braid status](status.md) -- detailed pool state including operation progress
- [braid lock](lock.md) -- take the pool offline
