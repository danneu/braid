# Remove dead bootstrap superblock check

## Context

In `cli/src/add.rs`, the bootstrap path (`!pool.mounted`) checks `device_has_btrfs_superblock` on mapper paths (lines 491–506). This check is unreachable: PresentLuks disks are rejected at lines 367–373 when the pool isn't mounted, so only freshly LUKS-formatted PresentNotLuks disks reach the bootstrap path — they can never have btrfs superblocks.

Removing this dead code is safe, but the invariant that makes it dead (line 367 rejection) must be locked with a regression test so it can't silently disappear in a future refactor.

## Changes

**`cli/src/add.rs`**

1. Delete lines 491–506 (the `any_has_superblock` loop and error branch)
2. Remove `device_has_btrfs_superblock` from the import on line 5 (no other call sites in this file)
3. Add a `cmd_add`-level test: `bootstrap_rejects_braid_labeled_luks_disk`

The `pub fn device_has_btrfs_superblock` in `luks.rs` stays — it's a general utility.

### New test

Add a test near the existing `cmd_add` journal tests (after line 1595) using `MockRunner` + `AddMockFs` (both already in scope). The test:

- Sets up a PresentLuks disk with mapper_open=false: by-id path exists in `AddMockFs`, no mapper path. Mocks only `CryptsetupLuksUuid` (returns UUID), `CryptsetupLuksDumpText` (returns `braid-disk2`), and `FindmntJson` (exit 1 → unmounted pool).
- Calls `cmd_add` and asserts the error contains `"bootstrap only accepts fresh disks"`

Minimal setup — the rejection fires before mapper state is checked, so the test should not depend on it.

This locks the invariant at line 367–373 at the `cmd_add` execution level. (The dry-run path already has `dry_run_braid_labeled_no_pool_reports_blocked` covering `compile_add_steps_multi`, but that's a separate code path.)

## Verification

- `just test-rust` — compilation + all unit tests pass, including the new regression test
