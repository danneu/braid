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

`btrfs replace` is the motivating example. Upstream btrfs explicitly warns that suspend/hibernate can interrupt device replace and recommends inhibiting sleep before running it. On newer kernels, suspend can cancel the replace outright; on older kernels, suspend can leave braid to recover a broken topology after wake.

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

### `braid replace`

`braid replace` holds a `block` sleep inhibitor only after interactive/reversible work is complete and just before the mutation window starts.

The protected scope includes:

- journal write
- new-disk LUKS initialization/open
- `btrfs replace start`
- post-replace `maybe_restore_raid1` soft balance for missing-path replacements

The protected scope excludes:

- `--dry-run`
- confirmation prompt
- passphrase reads
- reversible validation and identity checks

This matches braid's journal rule: the inhibitor is acquired before the first irreversible step, but failure to acquire it must not strand the user in recovery mode for a purely environmental error.

## Consequences

- suspend is blocked only when interruption is actually dangerous
- operators are not prevented from suspending the host while braid is still waiting on human input
- future long-running commands should reuse the same boundary rule instead of inventing command-specific behavior

Likely future candidates include explicit balance-style commands or other long-running pool mutations. The same default does not automatically apply to every long-running task; the deciding question is whether suspend would make the operation incorrect, unsafe, or expensive to restart.
