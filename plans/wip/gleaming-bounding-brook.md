# Fix: move journal write in cmd_add to after identity validation

## Context

The previous fix (delete JournalGuard, journal persists on error) exposed a latent issue: `cmd_add` writes the journal *before* the LUKS identity checks for `PresentLuks` disks. When a disk fails identity validation (e.g. `BraidLabeledNoBtrfs`), no irreversible operation happened, but the journal persists and blocks all subsequent commands until `braid recover` is run.

The failing VM test: "Braid-labeled LUKS with no btrfs is refused" → error → wipe disk → retry add → blocked by stale `pending-op.json`.

## Fix

Split the LUKS phase loop in `cmd_add` (`cli/src/add.rs:363-456`) into two passes:

**Pass 1 — Validate PresentLuks** (before journal write, lines ~363-456 PresentLuks arm only):
```
for each disk:
    if PresentLuks:
        read LUKS label → reject if non-braid
        check pool mounted
        open mapper if closed (tracked by luks_guard for cleanup)
        classify_braid_disk_fsid → error / AlreadyInPool / Recoverable
        if Recoverable → needs_pool_add.push(i)
    // PresentNotLuks: skip (handled in pass 2)
```

**No-op check** (moved here from after the loop):
If no PresentNotLuks disks exist AND `needs_pool_add` is empty → all disks are AlreadyInPool. Return Ok without writing journal. Remove the `clear_journal` call on this path since no journal was written.

**Write journal** — all validation passed, irreversible operations are next.

**Pass 2 — Execute PresentNotLuks** (after journal write, lines ~370-390 PresentNotLuks arm only):
```
for each disk:
    if PresentNotLuks:
        luks_format (irreversible)
        backup_luks_header
        ensure_luks_open → luks_guard.track
        enroll_key_file (optional)
        needs_pool_add.push(i)
    // PresentLuks: already validated in pass 1, skip
```

`luks_guard` spans both passes — created before pass 1, disarmed after pass 2. This correctly handles cleanup: if pass 1 opens a mapper for FSID verification and pass 2 fails, the guard closes all opened mappers.

## Behavioral improvement

Mixed-disk adds are now safer. If disk1 is PresentNotLuks and disk2 is PresentLuks+BraidLabeledNoBtrfs, the current code formats disk1 (irreversible) then fails on disk2. With the split, disk2's identity failure is caught in pass 1 before disk1 is ever formatted.

## Files

- `cli/src/add.rs` — split LUKS phase loop, move journal write between passes
- `cli/src/add.rs` tests — update `journal_cleared_on_noop_add` (rename to `no_journal_on_noop_add`, journal is never written so no clearing needed)

## Verification

1. `just test-rust` — all unit tests pass
2. `just test braid-add-disk` — the failing VM test passes
