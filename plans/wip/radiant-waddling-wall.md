# Fix: cmd_add writes journal before identity validation

## Context

The journal-guard fix (previous commit) made `pending-op.json` persist on error exits. This exposed a latent issue in `cmd_add`: the journal is written at line 356 before the LUKS identity checks at lines 391-454. When a `PresentLuks` disk fails identity validation (e.g. `BraidLabeledNoBtrfs`), no irreversible operation happened, but the journal persists and blocks all subsequent commands.

Failing VM test (`braid-add-disk`): "Braid-labeled LUKS with no btrfs is refused" → error leaves stale `pending-op.json` → next "Fresh disk4 added after cleanup" subtest is blocked.

## Fix

Split the single LUKS-phase loop (`add.rs:363-456`) into two passes so that identity validation completes before the journal is written.

### Pass 1 — Validate PresentLuks (before journal write)

Loop over `probed`, handle only `PresentLuks` disks (skip `PresentNotLuks`):
- Label check (line 393-402)
- Pool-mounted check (line 404-411)
- Open mapper if closed + track in `luks_guard` (line 413-418)
- `classify_braid_disk_fsid` (line 420-453)
- Recoverable → `needs_pool_add.push(i)`

`luks_guard` is created before this pass so it can clean up mappers opened for FSID verification if an identity check fails.

### No-op early return (moved from line 461)

After pass 1, check: are there any `PresentNotLuks` disks OR any entries in `needs_pool_add`? If neither, all disks are `AlreadyInPool` — disarm `luks_guard` (so mappers opened for FSID verification stay open) and return Ok. No journal was written, so no `clear_journal` needed.

### Write journal (stays at current position, just moved after pass 1)

### Pass 2 — Execute PresentNotLuks (after journal write)

Loop over `probed`, handle only `PresentNotLuks` disks (skip `PresentLuks`):
- `luks_format` (irreversible — journal already on disk)
- `backup_luks_header`
- `ensure_luks_open` + `luks_guard.track`
- `enroll_key_file`
- `needs_pool_add.push(i)`

Then `luks_guard.disarm()` and continue to pool phase as before.

### Bonus: mixed-disk safety

Current code: if disk1=PresentNotLuks and disk2=PresentLuks+BraidLabeledNoBtrfs, disk1 gets formatted (irreversible) before disk2's identity failure is caught. With the split, disk2 fails in pass 1 before disk1 is ever formatted.

## Test changes in `cli/src/add.rs`

- Rename `journal_cleared_on_noop_add` → `no_journal_on_noop_add`. The journal is now never written on the no-op path, so the test is simpler: assert `load_journal` returns `None` (no clearing logic to test).
- Add `no_journal_on_identity_failure`: configure `AddTestRunner` so `classify_braid_disk_fsid` returns `BraidLabeledNoBtrfs` (the disk has a matching braid label but no btrfs superblock). Call `cmd_add`, assert it returns Err AND `load_journal` returns `None`. This is the direct regression test for the stale-journal bug.

## Files

- `cli/src/add.rs` — split LUKS phase loop, move journal write

## Verification

1. `just test-rust` — all unit tests pass
2. `just test braid-add-disk` — the failing VM test passes
