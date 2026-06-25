---
intent: Document when braid should hold a systemd sleep inhibitor and, just as importantly, when it should not. Read before adding or moving `systemd-inhibit` usage around long-running operations.
status: Active
---

# Inhibit Sleep During Non-Interruptible Operations


> Principles:
> - [Safe-by-construction operations](../principles.md#3-safe-by-construction-operations)
> - [Sane defaults](../principles.md#7-sane-defaults)

## Context

braid enables whole-system suspend via autosuspend. That is the right default for a quiet, low-power NAS, but it creates a failure mode for long-running storage operations that should not be interrupted mid-flight.

`btrfs replace` is the motivating example. Upstream btrfs explicitly warns that suspend/hibernate can interrupt device replace and recommends inhibiting sleep before running it. On newer kernels, suspend can cancel the replace outright; on older kernels, suspend can leave braid to recover a broken topology after wake. The same risk profile applies to `btrfs device remove` (long-running data migration) and to the conditional balances in `add` and `remove-missing` (`pool_balance_raid1` after add to ≥2 disks; `maybe_restore_raid1` after clearing the last missing device).

braid needs a clear rule for when to hold a sleep inhibitor, because "just acquire it for the whole command" is too broad:

- It is unnecessary and user-hostile to block suspend while waiting for confirmation or passphrase entry.
- It is correct to block suspend once the command is entering the non-interruptible mutation window where interruption risks corruption, degraded topology, or restarting hours of work.

## systemd guidance

braid follows systemd's inhibitor model directly:

- `systemd-inhibit` is for work that should not be interrupted, such as recording media or similarly sensitive long-running operations.
- `block` inhibitors are for cases where sleep must be refused outright while the critical section is active.
- `delay` inhibitors are for short grace periods where a service needs time to prepare for sleep, not for hours-long work.
- Inhibitors should be held only for the shortest window that actually needs protection.

Primary references:

- `systemd-inhibit(1)`: <https://www.freedesktop.org/software/systemd/man/systemd-inhibit.html>
- systemd Inhibitor Locks: <https://systemd.io/INHIBITOR_LOCKS/>

## Decision

braid acquires a `What=sleep`, `Mode=block` inhibitor only for the non-interruptible portion of a long-running operation.

The inhibitor boundary is:

1. Run interactive prompts, passphrase collection, and reversible validation first.
2. Acquire the sleep inhibitor immediately before the irreversible mutation window begins.
3. Keep it held for the full duration of the non-interruptible work, including any required follow-up work that is part of the same intent command.
4. Release it immediately when that critical section ends, whether by success, error, or signal-driven unwind.

braid must not hold a sleep inhibitor during:

- confirmation prompts
- passphrase entry
- dry-run output
- reversible preflight that can fail without leaving partial state

## Current application

`braid replace`, `braid remove`, `braid remove-missing`, and `braid add` all hold a `What=sleep, Mode=block, Who=braid` logind inhibitor for their respective mutation windows. Each command acquires the inhibitor immediately before `journal::write_journal()`, after all interactive/reversible work, and holds it until the function returns (success, error, or signal-driven unwind).

For all four commands, the protected scope is the post-journal critical section, and the excluded scope is the same:

- `--dry-run`
- confirmation prompt
- passphrase reads
- reversible validation and identity checks

Failure to acquire the inhibitor returns a `Validation`-shaped error before the journal is written, so an environmental logind failure does not strand the user in recovery mode.

### `braid replace`

The protected scope includes:

- journal write and post-commit phase rewrite
- new-disk LUKS initialization/open
- `btrfs replace start`
- best-effort old-mapper close for live replacements
- post-replace resize
- post-replace soft RAID1 balance for missing-path replacements that clear the last missing device

The new-target LUKS identity check is deliberately **two-tier**: the primary gate (`cli/src/replace.rs#verify_existing_luks_new_target_preflight`) runs pre-journal under the excluded "reversible validation and identity checks" rule above, so an operator disk-swap or backing-drift in the post-confirmation window aborts on the reversible side without stranding `pending-op.json`; a residual re-probe (`probe_existing_luks_new_target_uuid` closed-mapper arm, `verify_existing_luks_open_mapper_target` open-mapper arm) stays post-journal inside the "new-disk LUKS initialization/open" scope to guard the narrow journal->open window that contains the optional slot-1 keyfile enroll. Do not collapse it to one tier.

### `braid remove`

The protected scope includes:

- journal write
- the optional pre-remove `pool_balance_single` (RAID1→single) when only one device will remain
- `btrfs device remove` data migration
- post-remove LUKS mapper close and membership persistence

### `braid remove-missing`

The protected scope includes:

- journal write
- `btrfs device remove <devid>` (chunk relocation via
  `btrfs_shrink_device`; can run for minutes when the missing device had data
  allocated because surviving RAID1 stripes are rewritten into newly allocated
  chunks on remaining devices)
- post-op membership persistence
- post-commit phase rewrite
- the conditional soft RAID1 balance that converts single-profile chunks (created during degraded operation) back to RAID1 when clearing the last missing device on a multi-disk pool

The inhibitor is acquired unconditionally before journal write, even in the cases where `maybe_restore_raid1` will be a no-op. This keeps the boundary rule simple ("acquire before journal") and matches the rest of the suite. The "savings" of skipping acquisition when the soft balance will not run are tiny on a NAS that is idle most of the time.

### `braid add`

The protected scope includes:

- journal write
- LUKS format/header backup/open of fresh disks
- `pool_bootstrap_mount` / `pool_bootstrap_mount_raid1` (bootstrap path) or `pool_add_device` followed by the conditional `pool_balance_raid1` (add-to-existing-pool path) when the post-add pool has ≥2 devices
- post-op membership persistence

As with `remove-missing`, the inhibitor is acquired unconditionally before journal write. The bootstrap path's mkfs phase is fast but still irreversible across the journal boundary; the add-to-existing path's RAID1 balance is the long-running phase that the inhibitor primarily protects.

The no-op early-return path (all requested disks already in the pool) returns before the inhibitor seam fires — no journal is written, so no protection is required.

`braid recover` follows the same boundary for replayed destructive work. In particular, add `PoolMutation` recovery resolves and verifies the needed passphrase before acquiring a sleep inhibitor; the inhibitor is acquired only after reversible credential checks pass and immediately before replaying target preparation or btrfs membership work.
Bootstrap-add `GenericLivePool` recovery likewise acquires the inhibitor immediately before replaying its owed post-add RAID1 soft balance, so every recover balance-replay path holds the inhibitor across the interruptible btrfs work.

### Excluded: `braid lock`

`braid lock` deliberately does not acquire the sleep inhibitor, even
though its mutation window (umount + per-mapper `cryptsetup close`) is
non-trivial in wall-clock time. This is the worked example of the
deciding question below applied to lock work specifically:

- **Recoverability.** A lock interrupted mid-flight leaves a state that
  re-running `braid lock` advances on, to the extent its existing
  probes can detect. Specifically:
  - `plan_lock`'s `mountpoint -q` skips the umount step when the pool
    is already unmounted (`cli/src/lock.rs`'s `plan_lock`).
  - The per-mapper close path checks `fs.exists("/dev/mapper/<name>")`
    before issuing `cryptsetup close` and reports "already closed"
    otherwise, so closed membership mappers do not re-error on a
    follow-up run.
  - Orphan mappers (`braid-*` paths not in `pool.json`) are re-scanned
    on each invocation and closed; close failures still surface as
    fatal errors, and a `/dev/mapper` scan failure is warned and
    yields an empty orphan list for that run -- not silently swallowed.

  Unlike `replace`/`add`/`remove`/`remove-missing`, there is no
  kernel-level topology corruption window and no hours-long restart
  cost. The point is that a partially-completed lock does not poison
  subsequent invocations -- not that every failure is hidden.
- **Shutdown-driven `ExecStop`.** When `braid lock` runs as
  `braid-online.service`'s `ExecStop=` during system shutdown, the
  system is heading to `shutdown.target`/power-off, not to suspend. A
  sleep inhibitor acquired during that window is redundant -- logind
  does not schedule a suspend transition mid-shutdown.
- **Manual stop and user-lock reentry.** `ExecStop=braid lock` also
  fires on a manual `systemctl stop braid-online.service` and on the
  Rust dispatch post-lock `mark_offline`
  (`cli/src/online_state.rs`) for user-initiated `braid lock`, gated
  on `systemd_lifecycle` (see
  [ADR 018's Rust dispatch synchronization](018-systemd-lifecycle.md#rust-dispatch-as-synchronization-layer) and
  `modules/braid/storage.nix`'s `braid-online` definition). Those
  paths do not enjoy the shutdown-driven guarantee above; their
  justification is the recoverability + short-duration argument, not
  the shutdown-target one.
- **Suspend context.** `braid-online.service` has no
  `Conflicts = sleep.target` (see `modules/braid/storage.nix`). By the
  `016-auto-suspend.md` design the pool stays mounted across suspend,
  so the only realistic mid-lock-suspend race is a user-initiated
  `braid lock` colliding with autosuspend's idle countdown. That
  window is narrow (lock is short) and the failure mode is
  recoverable, per the first bullet.
- **`ExecStop` budget.** `braid-online.service` runs lock under
  `TimeoutStopSec = 5min`. Adding subprocess work to that path (a
  `systemd-inhibit` fork plus its supervised `sh + sleep` child) buys
  no protection commensurate with the added shutdown-path complexity.

If a future change makes lock's mutation window genuinely long
(e.g. a multi-minute pre-lock balance), revisit this exclusion under the
same deciding question.

### Excluded: `braid enroll`

`braid enroll` does not acquire the sleep inhibitor despite mutating
LUKS slot 1 on each pool disk. Applying the deciding question to
standalone enroll specifically:

- **No journal, no recovery-mode lockout.** Standalone enroll writes no
  operation journal (`EnrollPlan::execute` in
  `cli/src/enroll_key_file.rs`). Suspend mid-loop cannot strand the
  operator in recovery mode, which is the failure surface this doc's
  "Validation-shaped error before journal write" promise protects
  against for the four inhibitor-using commands.
- **Recoverability.** `plan_enrollment` probes each candidate via
  `probe_keyfile_enrollment` and short-circuits disks whose slot 1
  already verifies the keyfile (`AlreadyEnrolled`). A partial enroll
  leaves only the un-enrolled disks for the next invocation: re-running
  `braid enroll DIR` (existing-keyfile mode) advances on partial state,
  the same property that justifies `lock`'s exclusion. Note that
  `braid enroll --generate` is not same-command idempotent -- a
  partial `--generate` run leaves `DIR/braid.key` on disk, and
  `validate_key_file_path` refuses a second `--generate` against an
  already-present keyfile. Recovery for an interrupted `--generate` run
  is to drop `--generate` and re-run as a regular enroll against the
  now-existing keyfile.
- **Bounded mutation window.** Each disk pays one Argon2-bounded
  `cryptsetup luksAddKey` (about 2-3 sec on default parameters) plus a
  sub-second `cryptsetup luksHeaderBackup`. A three-disk pool's total
  enroll window is single-digit seconds with no long-running btrfs work
  to protect.
- **No btrfs topology mutation; LUKS2 writes use cryptsetup metadata
  locking.** Enroll does not touch btrfs membership or chunk allocation,
  which is the topology-corruption risk surface this doc was written to
  protect. LUKS2 metadata writes are serialized by cryptsetup's own
  metadata locking. After each successful `cryptsetup luksAddKey`,
  `apply_enrollment` writes a local `.luksheader` as input to the
  existing off-system backup workflow (see `docs/internals/luks-unlock.md`); the
  local file is a transient byproduct of a successful mutation, not a
  recovery mechanism for an interrupted one. Recovery from actual header
  damage uses the operator's off-system backup, identical to every other
  LUKS-mutating command in braid.

The same `luks::enroll_key_file` call is held under an inhibitor when
invoked from `braid add --enroll` or `braid replace --enroll`, but that
is incidental: those commands already hold an inhibitor for their
journal-protected btrfs work, and the keyfile call happens inside that
existing window. Standalone `braid enroll` has no btrfs work to protect
and no journal boundary to guard, so an inhibitor would buy nothing.

If a future change adds long-running follow-up work to `braid enroll`
(e.g. a pool-wide rekey or a balance after enrollment), revisit this
exclusion under the same deciding question.

## Consequences

- suspend is blocked only when interruption is actually dangerous
- operators are not prevented from suspending the host while braid is still waiting on human input
- `add`, `remove`, `remove-missing`, and `replace` all follow the same boundary rule; future long-running commands should reuse it instead of inventing command-specific behavior
- failure to acquire the inhibitor (e.g. logind unreachable) is a clean validation error before the journal is written, never a recovery-mode lockout

The same default does not automatically apply to every long-running task; the deciding question is whether suspend would make the operation incorrect, unsafe, or expensive to restart.
