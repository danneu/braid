[← braid](../index.md)

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
| **0** | Pool is idle, or pool is offline |
| **1** | Pool is busy (running op) or pool state could not be determined |
| **2** | Setup error -- config could not be read |

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
busy: unknown (<probe>: <error>)
```

Only the scrub line carries a percentage. The named btrfs operation states
come from scanning `/sys/fs/btrfs/*/exclusive_operation`, which reports the
active operation but not its progress.

`busy: unknown (<probe>: <error>)` is printed when a probe failed. The
probe label is `mountinfo` for `/proc/self/mountinfo`, `sysfs` for
`/sys/fs/btrfs/*/exclusive_operation`, or `scrub` for `btrfs scrub status`
command/parser failures. The error text preserves the underlying diagnostic.

When the pool is offline (not mounted), exit code is 0 -- there is nothing to protect, so suspend is safe.

`braid idle` must run as root. A non-root invocation exits 1 with
`error: braid must be run as root` on stderr before config loading or any probe
runs, with no stdout output. The streams disambiguate this from the documented
exits above: exit 0 prints `idle:` on stdout, busy/probe-failure exit 1 prints
`busy:` on stdout, and config-load exit 2 emits a config-error diagnostic.

## Autosuspend integration

braid idle is designed to be used as an [autosuspend](https://autosuspend.readthedocs.io/) check. Add it to your autosuspend configuration:

```ini
[check.BraidIdle]
enabled = true
class = ExternalCommand
command = ! timeout -k 2 10 braid idle
```

Each piece of the block is load-bearing:

- `enabled = true` is required. autosuspend's raw-INI parser defaults `enabled` to `false` and silently skips the section otherwise. (The NixOS module submodule defaults this to true, which is why the in-tree module form omits it.)
- `class = ExternalCommand` is the exported activity-check class in autosuspend's plugin registry. It runs `command` via the shell and treats exit 0 as "activity detected" (block suspend), non-zero as "no activity" (allow suspend).
- The leading `!` inverts `braid idle`'s exit codes so autosuspend sees what it expects:
  - `braid idle` exits 0 (idle) -> `!` -> 1 -> autosuspend allows suspend
  - `braid idle` exits 1 (busy or probe failure) -> `!` -> 0 -> autosuspend blocks suspend
  - `braid idle` exits 2 (setup error) -> `!` -> 0 -> autosuspend blocks suspend (fail-closed)
- The inner `timeout -k 2 10` bounds signal-killable overruns -- e.g. a parser regression, a slow userspace probe, or network-FS latency -- by sending `TERM` after 10s and escalating to `KILL` two seconds later for processes that ignore or delay `TERM`. It must be *inside* the `!`-inverted command, not outside it: an overrun produces a non-zero exit which `!` then flips to 0, preserving the fail-closed invariant. An outer timeout would fail open because the shell gets killed before `!` runs. Uninterruptible kernel waits (a process stuck in `D` state) are out of scope for `timeout(1)`; autosuspend stalls until the syscall returns and the system stays awake by virtue of not deciding.

## What happens under the hood

1. Checks if the pool is mounted (via `/proc/self/mountinfo`)
2. If not mounted: returns idle (exit 0)
3. Reads `/sys/fs/btrfs/*/exclusive_operation` for any active exclusive operation on any btrfs filesystem: `balance`, `balance paused`, `device add`, `device remove`, `device replace`, `resize`, `swap activate`
4. If sysfs reports a busy operation or the sysfs probe fails, returns immediately before probing scrub
5. Probes scrub status via `btrfs scrub status` only after the sysfs scan is clean (scrub is not in the kernel exclusive-operation set, so sysfs cannot detect it)

When the host has more than one btrfs filesystem (e.g. a btrfs root in addition to the pool), an exclusive op on any of them keeps the system awake while the pool is mounted, and the `busy:` line above may name an op on the non-pool fs. This is intentionally conservative -- see [ADR 016: Auto-Suspend](../design/decisions/016-auto-suspend.md).

## Related commands

- [braid status](status.md) -- detailed pool state including operation progress
- [braid lock](lock.md) -- take the pool offline
