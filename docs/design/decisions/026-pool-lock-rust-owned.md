---
intent: Record why pool-operation locking and braid-online lifecycle synchronization live in Rust dispatch instead of the shell wrapper. Read before changing locked command dispatch, ExecStop behavior, or braid-online activation/deactivation.
status: Active
---

# Decision: Pool Lock Is Rust-Owned


> Principles:
> - [One pool operation at a time](../principles.md#12-one-pool-operation-at-a-time)
> - [Systemd Lifecycle State Machine](018-systemd-lifecycle.md)
> - [Inhibit Sleep](019-inhibit-sleep.md)

## Context

The shell wrapper originally serialized selected CLI operations by taking
`/run/braid-pool.lock` before execing the Rust binary. It also performed
post-success lifecycle work such as mount-point permissions and
`braid-online.service` activation/deactivation.

That split created two sources of truth:

- The wrapper had to know which Rust subcommands mutate pool state.
- Rust had to know which commands read `pool.json`, prompt, probe devices, or
  write recovery journals.

The lists drifted. `lock` and `enroll` needed the same early serialization as
other mutators but were not naturally owned by the wrapper's subcommand case
logic. Wrapper ownership also made it easier for Rust dispatch to grow a
pre-lock state read later, which would violate the stale-state invariant.

## Decision

Rust dispatch (`cli/src/main.rs`) owns pool-operation locking. The
`dispatch_policy` function in `cli/src/main.rs` is the single source of truth
for mapping `Commands` variants to lock acquisition disciplines, and for which
commands recovery mode refuses. Its wildcard-free exhaustive match makes every
new subcommand choose a discipline at compile time. For commands whose policy
acquires the pool lock, dispatch acquires `/run/braid-pool.lock` before reading
the recovery journal, loading config, loading membership, probing pool state,
prompting, or writing journals.

The shell wrapper is a pure exec shim. It only sets the module-controlled
`PATH` and execs the packaged Rust binary.

`braid-online.service` uses a distinct shutdown entry point:

```text
braid lock --systemd-stop --deadline-secs <n>
```

The module option `braid.lockSystemdStopDeadlineSecs` controls `<n>`. Its
default is 270 seconds, and the module asserts that it is strictly below
`braid-online.service` `TimeoutStopSec` (300 seconds).

Lifecycle work also lives under the Rust-held pool lock:

- After every real-run `unlock`, `add`, and `recover` attempt that gets past
  dispatch's early exits (pool-lock contention, the recovery gate, and config
  and pool.json loads), dispatch runs `mark_online` as a finalizer, success or
  failure. The `is_mountpoint` gate inside
  `mark_online` short-circuits when the operation failed before mounting; the
  bootstrap-add and recover cases where the mount succeeded but a later step
  returned `Err` are exactly where this finalizer matters.
- After every plain `braid lock` attempt, success or failure, dispatch runs
  an offline finalizer (`cli/src/lock.rs#cmd_lock_orchestrate`). It probes
  the mount once; only when the pool is unmounted does it write `done\n` and
  run `mark_offline`. A mapper close that fails after a successful umount
  therefore still stops `braid-online.service`; see
  [Stop Coordinator + Done Protocol](#stop-coordinator--done-protocol).
- The lock path stops lifecycle-bound scrub units and `BoundBy`
  `braid-online.service` consumers before unmounting.

Systemd lifecycle synchronization is gated by `systemd_lifecycle = true` in
runtime config. `modules/braid/cli.nix` emits that flag for module-managed
installs; standalone CLI configs omit it and therefore skip
`braid-online.service`, scrub-unit, and `BoundBy` systemctl calls. The pool
lock and `pool_access_group` mount-root permission fixups still run outside
that gate.

## Snapshot Rule On `systemctl start`

`mark_online` snapshots `braid-online.service` `ActiveState` at the start of
the pool-lock window. It starts the unit only if the snapshot was `inactive` or
`failed`.

It must skip `active`, `activating`, and `deactivating`. The `deactivating`
case is load-bearing: if a stop job is already running and its `ExecStop`
needs the pool lock, a new `systemctl start braid-online.service` would queue
behind that stop. If the caller already holds the pool lock, the queued start
can deadlock against the in-flight stop. Snapshot gating prevents the start
from being queued in that state.

Unknown snapshot results warn instead of starting. The pool remains mounted and
usable, but automatic shutdown cleanup may be missing.

## Lock Tolerates Missing Or Corrupt Membership

Lock-side dispatch loads pool membership from `pool.json` only; it consults no
recovery journal. If `pool.json` is missing, unreadable, corrupt, or fails its
uniqueness checks, lock does not abort -- it warns and proceeds with empty
membership. On the live plain-lock and `braid-online.service` ExecStop paths
the warning goes to stderr; under `--dry-run` it is folded into the stdout
preview to preserve the single-stream dry-run contract
([ADR 022](022-dry-run-preview-model.md)).

Membership is advisory for lock, not authoritative -- its only role here is to
attach friendly member names to status output. What lock closes is decided from
observed state, not from `pool.json`:

- mappers backing the live mounted pool, including members whose backing
  device is gone (`device: (null)`), each identified during the per-device
  probe by the LUKS UUID its dm UUID records;
- otherwise-stranded `/dev/mapper/braid-*` mappers, each confirmed by the LUKS
  UUID its dm UUID records (see [ADR 024](024-luks-uuid-identity.md)) before it
  is closed.

With empty membership these mappers classify as unnamed orphans rather than
named members and are still closed. Fallback scanning is limited to
`/dev/mapper/braid-*`; mounted-pool cleanup closes only the mapper paths
reported by the pool mounted at the configured mount point. A candidate that
fails verification or a `/dev/mapper` scan that fails is skipped with a warning
and may leave cleanup incomplete -- the operator resolves it by re-running
`braid lock` or closing the unverified mapper by hand.

This closes the failed-bootstrap-add lifecycle hole without a journal. A
bootstrap add can mount the pool and open its LUKS mappers, then fail before
braid writes the first `pool.json`. If shutdown follows,
`braid-online.service` ExecStop runs `braid lock --systemd-stop`, finds no
`pool.json`, and still unmounts and closes those mappers -- because what to
close is read from the live mounted pool and the observed mappers, not from
`pool.json`.

Lock therefore needs no special case for which operation was interrupted. An
interrupted `Remove`, `RemoveMissing`, `Replace`, or live-pool `Add` is
reconciled by `braid recover` against its `pending-op.json` journal; lock
neither reads nor needs that journal to perform safe shutdown cleanup.

## Stop Coordinator + Done Protocol

Plain `braid lock` acquires `/run/braid-stop-coordinator.lock` before the pool
lock. After `cmd_lock` returns, success or failure, it checks `mountpoint -q`.
If the pool is unmounted, it writes `done\n` to that coordinator file and then
synchronously stops `braid-online.service`. If the pool is still mounted, it
writes nothing and stops nothing. The order is probe, marker, stop, so `done\n`
is only ever written over an observed unmount.

`done\n` therefore means "plain lock has unmounted the pool and finished its
attempt; the reentry must not repeat it", not "every mapper closed". Lock
stops every bound consumer and the scrub timer before umount. If a later
close failed and `braid-online.service` stayed active, the next `braid unlock`
would see it active, skip the start, and leave those consumers off. Keying
the finalizer on the observed mount instead of on `cmd_lock`'s result means
no error after umount can leave the unit stale, including steps added later.

The recursive `ExecStop` reentry runs
`braid lock --systemd-stop --deadline-secs <n>`. If the stop coordinator is
held, the reentry polls for either:

- `done\n`, which means the plain lock already unmounted the pool and
  finished its attempt, and the reentry can exit 0 immediately.
- coordinator release without `done\n`, in which case it may proceed to acquire
  the pool lock and run the cleanup itself.
- deadline expiry, in which case it exits 1 before systemd's
  `TimeoutStopSec` can kill it.

This protocol replaces the stop-side snapshot gate from ADR 018 for plain
`braid lock`'s `mark_offline`. The synchronous stop is intentional: user
invocations should return only after `braid-online.service` is inactive, while
recursive `ExecStop` has a deterministic poll-out path instead of queuing
behind itself.

The `mountpoint -q` check runs before the marker, and `mark_offline` takes
its answer (`cli/src/online_state.rs#PoolUnmounted`) instead of probing again.
A check failure (e.g. `OnlineError::Spawn` mid-shutdown) counts as
still-mounted: plain lock warns, writes no `done\n`, and skips the stop,
leaving `braid-online.service` active. The operator can re-run `braid lock` or
`systemctl stop braid-online.service` to recover. This mirrors the
"unknown snapshot results warn instead of starting" rule from the
[Snapshot Rule On `systemctl start`](#snapshot-rule-on-systemctl-start)
section: when state is unknown, the fail-safe direction is to leave the
lifecycle owner active rather than deactivate over a possibly live pool. If
writing `done\n` fails, plain lock also skips the stop, because a synchronous
stop without the marker would leave the reentry polling the coordinator plain
lock holds.

Accepted trade-off: after a close failure, a leftover `braid-*` mapper stays
open with no filesystem on it. A concurrent `ExecStop` reentry exits 0 on
`done\n` instead of retrying that close, and later shutdowns do not run
`braid lock` because `braid-online.service` is inactive. btrfs is already
unmounted, so no data is at risk, and systemd-shutdown's final device-mapper
detach closes leftover mappers after it kills their holders. The operator
closes the mapper sooner by freeing its holder and re-running `braid lock`.

## Consequences

- There is a single source of truth for the locked-command list and acquisition
  discipline: `dispatch_policy` in Rust dispatch.
- The wrapper cannot drift from Rust command semantics because it no longer
  interprets subcommands.
- Lock acquisition is the first real execution boundary for covered commands.
  Environment failures such as lock contention happen before recovery journals.
- `mark_online` must keep the start-side snapshot rule to avoid the
  `deactivating` deadlock.
- `mark_offline` must keep the stop coordinator and `done\n` protocol because
  it deliberately uses synchronous `systemctl stop` after unmount.
- `braid-online.service` follows the observed mount on every plain-lock exit,
  so a lock that fails after umount still deactivates it and the next
  `braid unlock` restarts its consumers.
- The pool lock is independent from the sleep inhibitor. The lock prevents
  stale concurrent state reads; the inhibitor still protects only the
  non-interruptible mutation window described in [ADR 019](019-inhibit-sleep.md).
