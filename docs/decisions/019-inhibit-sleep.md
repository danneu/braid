---
intent: Document when braid should hold a systemd sleep inhibitor and, just as importantly, when it should not. Read before adding or moving `systemd-inhibit` usage around long-running operations.
---

# Inhibit Sleep During Non-Interruptible Operations

Status: Active

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

- journal write
- new-disk LUKS initialization/open
- `btrfs replace start`
- post-replace `maybe_restore_raid1` soft balance for missing-path replacements

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
- the conditional `maybe_restore_raid1` soft balance that converts single-profile chunks (created during degraded operation) back to RAID1 when clearing the last missing device on a multi-disk pool
- post-op membership persistence

The inhibitor is acquired unconditionally before journal write, even in the cases where `maybe_restore_raid1` will be a no-op. This keeps the boundary rule simple ("acquire before journal") and matches the rest of the suite. The "savings" of skipping acquisition when the soft balance will not run are tiny on a NAS that is idle most of the time.

### `braid add`

The protected scope includes:

- journal write
- LUKS format/header backup/open of fresh disks
- `pool_bootstrap_mount` / `pool_bootstrap_mount_raid1` (bootstrap path) or `pool_add_device` followed by the conditional `pool_balance_raid1` (add-to-existing-pool path) when the post-add pool has ≥2 devices
- post-op membership persistence

As with `remove-missing`, the inhibitor is acquired unconditionally before journal write. The bootstrap path's mkfs phase is fast but still irreversible across the journal boundary; the add-to-existing path's RAID1 balance is the long-running phase that the inhibitor primarily protects.

The no-op early-return path (all requested disks already in the pool) returns before the inhibitor seam fires — no journal is written, so no protection is required.

## Consequences

- suspend is blocked only when interruption is actually dangerous
- operators are not prevented from suspending the host while braid is still waiting on human input
- `add`, `remove`, `remove-missing`, and `replace` all follow the same boundary rule; future long-running commands should reuse it instead of inventing command-specific behavior
- failure to acquire the inhibitor (e.g. logind unreachable) is a clean validation error before the journal is written, never a recovery-mode lockout

The same default does not automatically apply to every long-running task; the deciding question is whether suspend would make the operation incorrect, unsafe, or expensive to restart.
