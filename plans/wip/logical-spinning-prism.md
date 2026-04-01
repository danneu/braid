# Plan: VM test for bootstrap crash recovery escape instructions

## Context

`recover.rs:62-110` has special handling for bootstrap crashes: when `pre_membership` is empty (first-ever `braid add`) and the target disks have no btrfs superblock, `braid recover` prints actionable escape instructions instead of a cryptic mount error. This path has 4 Rust unit tests with mocked runners but zero VM tests exercising it end-to-end against real LUKS/btrfs.

First-time users are the most likely to hit a bootstrap crash. If this code path regresses, they'd get a raw mount error instead of the "rm pending-op.json, wipefs, re-run braid add" instructions.

## Correction to the finding

The finding suggests "disks should be raw (no LUKS, no btrfs)" and "verify no LUKS mappers were opened." The first part is wrong — the code path requires:

1. LUKS open to **succeed** (disks must have LUKS headers)
2. Mount to fail with `MountFailed` (no btrfs superblock inside LUKS)
3. `btrfs filesystem show` probe on opened mappers to confirm `NoBtrfs`

If disks are raw (no LUKS), `open_and_mount_pool` would return `Failed("no unlockable disks found")` — a different error variant that doesn't trigger the bootstrap detection at all (`recover.rs:74` checks specifically for `MountFailed`).

The correct scenario simulates a crash **after LUKS format but before mkfs.btrfs**. And rather than verifying mappers are *closed*, we verify they are *open* — proving recover actually reached the LUKS-open → mount-fail → btrfs-probe branch.

## Test design: single disk

The existing unit test coverage (`recover.rs:838`) uses a single-disk bootstrap. A second disk adds setup cost and failure surface without exercising any additional recovery logic. One disk is sufficient to pin down the end-to-end path.

## Test scenario

1. LUKS format disk1 with a known passphrase (no mkfs.btrfs)
2. Close the LUKS container
3. Inject `pending-op.json` with `pre_membership: {disks: {}}` (empty) and `target_membership` containing disk1, `op: Add`
4. Run `braid recover --passphrase-stdin`
5. Verify: exits non-zero
6. Verify: stderr contains "bootstrap add was interrupted"
7. Verify: stderr mentions "pending-op.json" and "wipefs"
8. Verify: `/dev/mapper/braid-disk1` exists (proves LUKS open succeeded — recover reached the MountFailed→NoBtrfs branch, not an earlier failure)
9. Verify: `pending-op.json` still exists (journal preserved)
10. Verify: `pool.json` does NOT exist
11. Verify: `/mnt/storage` is NOT a mountpoint

## Files to create/modify

### 1. `tests/cli/recover-bootstrap-crash.nix` (new)

Required Intent/Why/Scenario block comment per AGENTS.md test conventions. Modeled on `tests/cli/braid-recover.nix`. One 512MiB empty disk, braid + cryptsetup + btrfs-progs packages, standard config.

### 2. `tests/cli/recover-bootstrap-crash.py` (new)

Required Intent/Why/Scenario block comment per AGENTS.md test conventions, then:

```
Phase 1 — Simulate interrupted bootstrap (LUKS format succeeded, no mkfs)
  - cryptsetup luksFormat disk1 with fast PBKDF
  - cryptsetup close to leave it locked

Phase 2 — Inject pending-op.json
  - pre_membership: {disks: {}}  (empty — the bootstrap signal)
  - target_membership: {disks: {disk1: {by_id: "/dev/disk/by-id/virtio-disk1"}}}
  - op: {op: "Add", disks: {disk1: "/dev/disk/by-id/virtio-disk1"}}

Phase 3 — Run braid recover, verify escape instructions
  - printf passphrase | braid recover --passphrase-stdin  must exit non-zero
  - Combined stdout+stderr must contain "bootstrap add was interrupted"
  - Must contain "pending-op.json" and "wipefs"
  - Must mention disk1's by_id path

Phase 4 — Verify state: recover reached the right branch, mutated nothing
  - /dev/mapper/braid-disk1 exists (LUKS was opened — proves we hit MountFailed path)
  - pending-op.json still exists
  - pool.json does NOT exist
  - /mnt/storage is NOT a mountpoint
```

### 3. `flake.nix` (modify)

Add registration block after the existing `braid-recover` entry (~line 309):
```nix
recover-bootstrap-crash = pkgs.testers.nixosTest (
  import ./tests/cli/recover-bootstrap-crash.nix {
    braid = linuxCrane.braid;
  }
);
```

## Key code references

- `cli/src/recover.rs:62-110` — bootstrap crash detection + escape message
- `cli/src/mount.rs:45-220` — `open_and_mount_pool` (line 209: `MountFailed` variant)
- `cli/src/journal.rs:13-21` — Journal struct (serde format)
- `cli/src/membership.rs:27-49` — PoolMembership/DiskMember (serde format)
- `tests/cli/braid-recover.nix` + `.py` — template for nix config and journal injection pattern

## Verification

```
just test recover-bootstrap-crash
```

On failure, add `-v` to see VM logs.
