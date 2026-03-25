# Plan: Always mount with `subvolid=5`

## Context

braid mounts the btrfs pool without specifying a subvolume, so it mounts whatever the **default subvolume** is. On a fresh filesystem that's the top-level (ID 5), but `btrfs subvolume set-default` can silently change this. If that happens, braid mounts a non-top-level subvolume and sibling subvolumes disappear from the mountpoint — a confusing, silent failure.

Hardcoding `subvolid=5` makes braid always mount the top-level subvolume, regardless of default subvolume changes. Pure safety — no behavior change for any current user.

## Changes

### 1. CLI base mount options

**File:** `cli/src/cmd.rs:204`

Add `"subvolid=5"` to `base_mount_options()` with a doc comment explaining why.

All four mount paths inherit from this function:
- Normal unlock → `CmdRequest::Mount` (cmd.rs:444)
- Degraded unlock → `CmdRequest::MountWithOptions` (cmd.rs:458)
- Single-disk bootstrap → `pool_bootstrap_mount` → `CmdRequest::Mount` (pool.rs:335)
- RAID1 bootstrap → `pool_bootstrap_mount_raid1` → `CmdRequest::Mount` (pool.rs:369)

### 2. CLI unit tests

**File:** `cli/src/cmd.rs:1155-1194`

Update two existing tests that assert exact option strings:
- `mount_includes_skip_balance` (line 1166): `"noatime,skip_balance"` → `"noatime,skip_balance,subvolid=5"`
- `mount_with_options_includes_skip_balance` (line 1189): `"noatime,skip_balance,degraded"` → `"noatime,skip_balance,subvolid=5,degraded"`

### 3. NixOS module fileSystems entry

**File:** `modules/braid/storage.nix:30-41`

Add `"subvolid=5"` after `"skip_balance"` with a comment explaining why:

```nix
"skip_balance"
# subvolid=5: always mount the top-level subvolume, regardless of
# btrfs subvolume set-default. Prevents silent mount target changes.
"subvolid=5"
```

### 4. VM test fixtures (11 files)

Each fixture manually copies the module's fileSystems options (because `qemu-vm.nix` clobbers `fileSystems` with `mkVMOverride`). Add `"subvolid=5"` after `"skip_balance"` in each:

| File | Line |
|------|------|
| `tests/module/bad-config.nix` | 43 |
| `tests/module/add-bootstrap.nix` | 45 |
| `tests/module/raid1.nix` | 72 |
| `tests/module/no-silent-degraded.nix` | 97 |
| `tests/module/auto-unlock-key-missing.nix` | 76 |
| `tests/module/single-disk.nix` | 59 |
| `tests/module/auto-unlock-key-wrong.nix` | 93 |
| `tests/module/degraded-raid1.nix` | 84 |
| `tests/module/auto-unlock-key-present.nix` | 113 |
| `tests/module/single-disk-dead.nix` | 62 |
| `tests/cli/braid-browse.nix` | 64 |

### 5. VM test assertion

**File:** `tests/cli/braid-unlock.py:85-87`

Add assertion that `subvolid=5` appears in `findmnt` output after unlock (alongside existing `skip_balance` check). This verifies the CLI → mount → kernel round-trip end-to-end.

This also serves as module-side verification: the `braid-unlock` test exercises the real `braid unlock` command, which calls `mount -o noatime,skip_balance,subvolid=5,...`. The `findmnt` check confirms the option reached the kernel.

### 6. README

**File:** `README.md:~292`

Add a sentence after the `skip_balance` explanation noting that braid always mounts `subvolid=5` (the top-level subvolume) so that `set-default` changes can't alter what gets mounted.

## Files to modify

| File | Change |
|------|--------|
| `cli/src/cmd.rs` | `base_mount_options()` + 2 unit tests |
| `modules/braid/storage.nix` | fileSystems options |
| `tests/module/bad-config.nix` | fixture options |
| `tests/module/add-bootstrap.nix` | fixture options |
| `tests/module/raid1.nix` | fixture options |
| `tests/module/no-silent-degraded.nix` | fixture options |
| `tests/module/auto-unlock-key-missing.nix` | fixture options |
| `tests/module/single-disk.nix` | fixture options |
| `tests/module/auto-unlock-key-wrong.nix` | fixture options |
| `tests/module/degraded-raid1.nix` | fixture options |
| `tests/module/auto-unlock-key-present.nix` | fixture options |
| `tests/module/single-disk-dead.nix` | fixture options |
| `tests/cli/braid-browse.nix` | fixture options |
| `tests/cli/braid-unlock.py` | findmnt assertion |
| `README.md` | document subvolid=5 |

## Verification

1. `just test-rust` — unit tests confirm `subvolid=5` in mount option strings for both `Mount` and `MountWithOptions` variants
2. `just test braid-unlock` — VM test confirms `subvolid=5` appears in live `findmnt` output after CLI unlock (end-to-end CLI + kernel verification)
3. Spot-check one or two other VM tests (e.g., `just test braid-browse`) to confirm the updated fixtures don't break anything
