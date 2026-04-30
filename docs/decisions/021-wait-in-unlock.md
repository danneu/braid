---
intent: Capture why `braid unlock` and the shared mount helpers
  announce every blocking step with a `[wait]` row, why the rule is
  not yet a project-wide principle, and which commands are bound by
  this ADR today. Read before changing unlock UX or the mount
  helpers.
---

# Wait rows in unlock and shared mount helpers

Status: Active

> Principles:
> - (none yet -- promote to a principle once the rest of the
>   interactive commands comply)

## Context

The single-passphrase invariant
([Principle 4](../principles.md#4-single-passphrase)) requires `braid
unlock` to verify the supplied credential against every reachable LUKS
member before opening any mapper. On a 3-disk pool that is three
sequential Argon2 derivations -- visible to the user as three
back-to-back `[wait] passphrase: checking against ...` rows.

Two later phases of the same command stayed silent:

1. **Per-disk `cryptsetup luksOpen`.** cryptsetup re-derives Argon2
   inside `luksOpen` even after `--test-passphrase` already verified.
   The user saw three `[ok] disk X: unlocked` rows arrive one by one
   with no leading announcement.
2. **Mount phase.** `scan_and_mount` runs `btrfs device scan` +
   `mkdir` + `mount` in sequence and emits a single `[ok] pool:
   mounted ...` row at the end. None of the three steps are
   announced individually.

The result: between the last verify row and the mount row, the user
stared at an inactive terminal for several seconds with no signal that
anything was happening.

## Options considered

1. **TTY spinner / progress bar.** Rejected: requires a TTY, fights log
   capture, and looks broken inside `braid-auto-unlock.service`
   journals.
2. **Best-effort ad-hoc waits.** Add `[wait]` rows whenever a gap is
   noticed. Rejected: gaps recur whenever a new slow path is added.
3. **Codify "[wait] before every blocking step" as a project
   principle now.** Rejected: principles are authoritative
   (`docs/principles.md:3`); a principle the codebase doesn't satisfy
   on the day it lands is a documentation bug. `add`, `replace`,
   `remove`, `remove-missing`, `recover`'s own (non-shared) slow
   paths, `enroll`, and `lock` keep silent gaps today.
4. **Scope the rule to `braid unlock` and the shared mount helpers
   today; promote to a principle once the other commands comply.**
   Accepted.

## Decision

`braid unlock` -- and `braid recover`'s mount tail, which routes
through the same shared helpers -- emit a `[wait]` row before every
blocking step:

- per-disk `cryptsetup luksOpen` (passphrase and keyfile arms),
  worded `[wait] disk {name}: unlocking...`;
- the mount phase (`btrfs device scan` + `mkdir` + `mount`),
  worded `[wait] pool: mounting {mount_point}...`. The single row
  covers all three steps because emitting one row per step would be
  noisy without buying the user any actionable information.

Each `[wait]` row uses
`status_tag::status_line(StatusTag::Wait, ...)` and is closed by the
existing per-step success row.

Other interactive commands keep their current behavior until they are
individually updated.

## Tradeoffs accepted

- Slightly more verbose stderr.
- Enforcement for the in-scope helpers is by VM-test assertion in
  `tests/cli/braid-unlock.py`, `tests/cli/braid-unlock-key-file.py`,
  and `tests/cli/braid-recover.py`. Project-wide enforcement is
  deferred until promotion to a principle.
- `braid recover` inherits the new rows automatically because
  `execute_mount_only` and `execute_unlock_and_mount` are shared.
  This is desirable: `recover`'s mount tail is exactly the same
  blocking work as `unlock`'s.

## Path to promotion

This ADR is intentionally narrow. Once every interactive command
listed below emits a `[wait]` row before its blocking work, promote
the rule to a numbered principle in `docs/principles.md`, mark this
ADR `Superseded` (pointing at the new principle), and update
`docs/index.md`'s "Twelve canonical invariants" wording.

Commands that still have un-announced blocking steps:

- **`braid add`** (`cli/src/add.rs`)
  - `luks_format()` (cryptsetup luksFormat -- ~10s+ per disk for
    Argon2); currently emits only `eprintln!("LUKS formatted:")`
    after the fact.
  - `pool_balance_raid1()` (btrfs balance to RAID1 -- potentially
    hours); currently emits only `eprintln!("Balancing to RAID1...")`.
    The progress module output is fine for live updates but the
    leading `[wait]` is missing.

- **`braid replace`** (`cli/src/replace.rs`)
  - `luks_format()` for the new disk.
  - `btrfs replace start` itself -- can run hours.
  - Soft balance after replacing a missing device.

- **`braid remove`** (`cli/src/remove.rs`)
  - Pre-remove RAID1 -> single balance (when shrinking from 2 to 1).
  - `btrfs device remove` itself.

- **`braid remove-missing`** (`cli/src/remove_missing.rs`)
  - Optional soft balance after clearing the last missing device.

- **`braid recover`** (`cli/src/recover.rs`) -- the parts NOT covered
  by the shared mount helpers:
  - Resume paused balance.
  - Replay RAID1 soft balance.

- **`braid lock`** (`cli/src/lock.rs`)
  - `umount` (can briefly block on fd closure / inhibitors).
  - `cryptsetup close` retry loop -- emits `[warn]` on busy retry but
    no leading `[wait]` for the close attempt itself.

- **`braid enroll`** (`cli/src/enroll_key_file.rs`)
  - Already emits `[wait]` for credential verify (good).
  - The actual `cryptsetup luksAddKey` (slot-1 keyfile enrollment)
    runs Argon2 again and is currently un-announced.

When a command is brought into compliance, strike its bullet from
this list in the same change. When the list is empty, do the
promotion.

## See

- `cli/src/mount.rs` -- `open_disks_with_passphrase`,
  `open_disks_with_key_file`, and `scan_and_mount` host the new rows.
- `cli/src/status_tag.rs` -- the canonical `StatusTag::Wait` and
  `status_line` helpers.
- `cli/src/unlock.rs:93-96` -- the already-mounted short-circuit
  that returns before any helper runs (so already-mounted unlocks
  emit no new rows).
