---
experimental: true
---
[← braid](../index.md)

# braid idle

{{#include ../_includes/experimental-command-callout.md.inc}}

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

Argument errors -- an unknown flag, a missing value -- also exit 2, but clap
writes its own `error:` diagnostic to stderr and never the
`failed to read/parse config file <path>` line a config-load exit 2 prints, so
the stderr text still tells the two exit-2 cases apart. The autosuspend check
runs a fixed argument list, so its only exit 2 is a config-load failure.

## Autosuspend integration

`braid idle` is the activity check behind braid's auto-suspend. You don't write
this check by hand: set `braid.autoSuspend.enable = true` and braid's NixOS
module generates the [autosuspend](https://autosuspend.readthedocs.io/)
`services.autosuspend` ExternalCommand check (`BraidPool`) for you. The
generated command -- `bash -c '! timeout -k 2 10 braid idle'`, with fully
qualified `/nix/store` paths for `bash`, `timeout`, and `braid` -- handles the
exit-code inversion autosuspend expects and a fail-closed inner `timeout`.
Don't reproduce it by hand: autosuspend runs the check outside braid's wrapper,
so bare `braid`/`timeout` are not on its PATH.

See the [power management guide](../guides/power-management.md) for setup, and
[ADR 016: Auto-Suspend](../design/decisions/016-auto-suspend.md) for the
exit-inversion table, the qualified-path requirement, and why `timeout` must
sit inside the `!`-inverted command.

## What happens under the hood

1. Checks if the pool is mounted (via `/proc/self/mountinfo`)
2. If not mounted: returns idle (exit 0)
3. Reads `/sys/fs/btrfs/*/exclusive_operation` for any active exclusive operation on any btrfs filesystem: `balance`, `balance paused`, `device add`, `device remove`, `device replace`, `resize`, `swap activate`
4. If sysfs reports a busy operation or the sysfs probe fails, returns immediately before probing scrub
5. Probes scrub status via `btrfs scrub status` against the configured pool mount point, only after the sysfs scan is clean (scrub is not in the kernel exclusive-operation set, so sysfs cannot detect it)

When the host has more than one btrfs filesystem (e.g. a btrfs root in addition to the pool), an exclusive op on any of them keeps the system awake while the pool is mounted, and the `busy:` line above may name an op on the non-pool fs. This is intentionally conservative -- see [ADR 016: Auto-Suspend](../design/decisions/016-auto-suspend.md). Scrub detection is narrower: `braid idle` only checks for a scrub on the braid pool itself, so a scrub running on a non-pool btrfs (e.g. the btrfs root) is not detected and does not block suspend.

## Related commands

- [braid status](status.md) -- detailed pool state including operation progress
- [braid lock](lock.md) -- take the pool offline
