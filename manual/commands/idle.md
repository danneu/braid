[← Manual](../index.md)

# braid idle

Check if the pool has any active operations. Designed for autosuspend integration.

## When to use it

- As an autosuspend check to prevent the system from sleeping during scrub, balance, or replace operations
- In scripts that need to wait for the pool to be idle before proceeding

## Basic example

```
sudo braid idle
```

Output:

```
idle: pool is idle
```

## Exit codes

| Exit code | Meaning |
|---|---|
| **0** | Pool is idle (no operations running), OR pool is offline |
| **1** | Pool is busy (scrub, balance, or replace is running) |
| **2** | Error (could not determine pool state) |

The busy reason is printed to stdout:

```
busy: scrub running (45%)
busy: balance running (70% left)
busy: balance paused (58% left)
busy: replace running (45.3%)
```

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
3. Checks scrub status (`btrfs scrub status`)
4. Checks balance status (`btrfs balance status`)
5. Checks replace status (`btrfs replace status`)
6. Returns busy on the first active operation found (short-circuits -- does not check remaining operations)

## Related commands

- [braid status](status.md) -- detailed pool state including operation progress
- [braid lock](lock.md) -- take the pool offline
