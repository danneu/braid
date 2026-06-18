# Plan: document why the remove-missing 2-disk guard may assert "RAID1"

## Context

A review finding (Low/Correctness) flagged that the 2-disk reject in
`plan_remove_missing` calls the pool "RAID1" in its refusal message while the
guard only checks device counts (`pool.total_devices == 2 && pool.devices.len()
== 1 && pool.missing_count == 1`), never the data/metadata profile. The finding
proposed softening the wording to be profile-neutral.

Investigation (`/verify-issue`) showed the wording is *correct* for every pool
braid manages, and the proposed rewrite is the wrong fix:

- **The RAID1 label reflects a real, documented invariant.** Per
  `docs/design/decisions/001-btrfs-raid1.md`, braid only ever stabilizes a
  2-device pool as RAID1 (a 1-drive pool is single; adding the 2nd converts to
  RAID1). A braid-managed 2-device pool is RAID1 by construction; the finding's
  "2-device single pool" is a non-braid filesystem.
- **The assumption is pervasive, not a stray string.** The whole module enforces
  RAID1: `restore_raid1_after_commit` rides the journal (`remove_missing.rs`
  around the `build_journal` call) and `maybe_restore_raid1` (`pool.rs`) soft-
  balances survivors back to RAID1 after recovery. A degraded RAID1 pool even
  holds *transient* single chunks from degraded writes (documented on
  `maybe_restore_raid1`), so it is never a stable single pool. A profile-neutral
  message would imply profile-agnostic code that does not exist.
- **Correctness does not rest on the pre-flight.** `device_remove_error`
  (`pool.rs`) decodes the kernel's actual "unable to go below" min-devices
  rejection profile-agnostically, for both live and missing removals. The
  pre-flight only upgrades the message and avoids stranding the journal/inhibitor.

The guard reads like an unverified assertion only because its comment cites the
kernel mechanism and ADR 012 (`012-intent-cli.md`) but never the invariant
(ADR 001) that makes the "RAID1" label safe. The right, minimal change is to
add that citation so this class of finding dissolves -- no wording, behavior,
test, or docs change.

## Change

**File:** `cli/src/remove_missing.rs` -- the comment block immediately above the
guard (`if pool.total_devices == 2 && pool.devices.len() == 1 && pool.missing_count == 1`).

Extend the existing comment (after the kernel-mechanism / ADR 012 text, before
the `if`) with a paragraph justifying the RAID1 label. Suggested text (implementer
may tighten; match the surrounding bare-symbol / plain-path comment style already
used for `btrfs_rm_device` and `012-intent-cli.md`):

```rust
// The "RAID1" label comes from device counts, not a probed profile
// (PoolState carries none by design). That is sound: per
// docs/design/decisions/001-btrfs-raid1.md braid only ever stabilizes a
// 2-device pool as RAID1 (1 drive = single; the 2nd converts to RAID1),
// so a braid 2-device pool is RAID1 by construction. Degraded writes can
// add transient single chunks, which maybe_restore_raid1 re-mirrors -- it
// is never a stable single pool. The runtime backstop is
// device_remove_error (pool.rs), which decodes the kernel's real
// min-devices rejection regardless of profile, so correctness never
// rests on this label.
```

## Explicitly NOT changing

- The refusal message string (`remove_missing.rs`, the `"cannot remove missing
  devid ... 2-disk RAID1 pool with one disk missing ..."` format) stays verbatim.
- No behavior change (the guard predicate is untouched).
- No test changes: the message-pinning assertions in `remove_missing.rs`
  (`single_survivor_rejected_at_preflight`, `single_survivor_rejected_in_dry_run`)
  and `tests/cli/remove-missing-2disk-rejected.py` still pass unchanged.
- No docs change: `docs/commands/remove-missing.md` and the rendered book are
  consistent because the wording is identical.

Rejected alternatives: (a) the finding's profile-neutral rewrite -- incoherent
(drops "RAID1" from the description but keeps it in the explanation) and
misrepresents a module that enforces RAID1; (b) probing the profile to verify the
guard -- over-engineering for a non-braid hypothetical, and incoherent unless the
whole module (`maybe_restore_raid1` et al.) is also made profile-aware.

## Verification

Comment-only, so the bar is "nothing drifted":

1. `just test-rust` (or `cargo test -p braid` for the CLI crate) -- the two guard
   regression tests above must stay green, confirming the message text did not
   change.
2. `cargo fmt --check` and `cargo clippy` -- comment formatting / no new warnings.
3. Spot-check: `git diff` touches only the comment block in
   `cli/src/remove_missing.rs`; no string-literal or logic lines appear in the diff.

## Implementation notes

- Used `just test-rust` for the test step because the workspace package is
  `braid-cli`, not `braid`; `justfile#test-rust` explicitly prefers that recipe
  over `cargo test -p <name>`.
