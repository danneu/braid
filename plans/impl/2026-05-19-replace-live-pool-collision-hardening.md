# Harden `braid replace` against execute-time live-pool UUID collisions

## Summary

Add an execute-time live-pool validation to `braid replace` so a
replacement target whose LUKS UUID was safe during planning is rejected
if that UUID appears in the mounted pool before execution reaches
`journal::write_journal` or `btrfs replace start`.

This mirrors the `braid add` hardening shape: do one fresh `probe_pool`,
verify the pool identity still matches the planned pool, then reject any
live-pool device whose `luks_uuid == new_uuid`.

No public CLI/API/schema changes.

## Key Changes

- Add a private replace-local helper near
  `verify_existing_luks_open_mapper_target` in `cli/src/replace.rs`.
  Inputs: runner, filesystem, mount point, planned `PoolState`, and
  `new_uuid`.
- The helper calls `probe_pool(runner, fs, mount_point)`, fails closed
  if the pool is no longer mounted, fails closed if the fresh pool FSID
  differs from the planned pool FSID, and returns
  `ReplaceError::DuplicateUuid { scope: DuplicateUuidScope::LivePool }`
  if any fresh live pool device has `luks_uuid == new_uuid`.
- Call the helper in `ReplacePlan::execute` after confirmation,
  passphrase read, and credential verification, but before sleep
  inhibitor acquisition and `journal::write_journal`.
- Run the helper for all target prep variants. Fresh targets should
  normally pass, but the uniform gate protects against stale planning
  state and keeps the invariant simple.
- Update comments around `verify_existing_luks_open_mapper_target`:
  that helper verifies the already-open mapper's backing path and UUID;
  the new helper verifies that no other live pool device has the
  replacement UUID.
- Update [docs/decisions/024-luks-uuid-identity.md](../../docs/decisions/024-luks-uuid-identity.md):
  extend the existing replace-related entries so the invariant covers
  the execute-time live-pool re-probe (not just planning-time and
  recovery checks), and list the new VM race test under "Tests That
  Enforce This".

## Test Plan

- Add Rust unit coverage in `cli/src/replace.rs`:
  - Fresh `probe_pool` contains `new_uuid` under a live device, using a
    `FreshLuks` `ReplacePlan` (the VM race below covers `ExistingLuks`,
    so together they pin both target prep variants through the uniform
    gate): `ReplacePlan::execute` returns
    `ReplaceError::DuplicateUuid { scope: LivePool }`, does not acquire
    the sleep inhibitor, does not write `pending-op.json`, and does not
    issue `BtrfsReplaceStart`.
  - Fresh `probe_pool` is unmounted: execution returns
    `ReplaceError::Validation` before journal write.
  - Fresh `probe_pool` has a different FSID: execution returns
    `ReplaceError::Validation` before journal write.
  - Fresh `probe_pool` has no `new_uuid`: execution proceeds past the
    new gate; use an intentional downstream failure and assert
    `pending-op.json` exists to prove the gate did not reject.
- Add a NixOS VM race test:
  - Build a healthy pool with disk1+disk2.
  - Prepare disk3 as an ExistingLuks replacement target.
  - Start interactive `braid replace --old disk2 --new disk3=...`
    without `--yes`.
  - While it waits at confirmation, clone disk3's LUKS header to disk4,
    open disk4 under a non-conflicting mapper such as `clone-foreign`,
    and run `btrfs device add /dev/mapper/clone-foreign /mnt/storage`.
  - Resume replace.
  - Assert non-zero exit, output contains `duplicate LUKS UUID` and
    `already present in live_pool`, `pool.json` is unchanged, and
    `pending-op.json` does not exist.
  - Register the new VM test in `flake.nix`.
- Verification:
  - `just test-rust`
  - `just test-vm replace-cloned-luks-header-rejected replace-new-in-pool-guard replace-live-pool-collision-race-rejected replace-live-disk replace-dead-disk`

## Assumptions

- This follow-up targets normal `replace` execution in
  `cli/src/replace.rs`; `recover.rs` replay hardening is out of scope
  unless a separate audit finds the same pre-journal window there.
- The residual race between the fresh probe and `btrfs replace start`
  remains out of scope because btrfs provides no atomic UUID guard at
  that boundary.
- Reuse `ReplaceError::DuplicateUuid { scope: LivePool }` for
  operator-visible consistency with the existing planning-time guard.
