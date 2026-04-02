# Fix: initrd fixture fails to load dm-crypt in NixOS VM tests

## Context

The `braid-module-degraded-raid1` test fails on native NixOS (x86_64-linux with KVM) but passed on macOS (aarch64-darwin via linux builder). The root cause is that `dm-crypt` isn't available in the initrd — the fixture service fails entirely, leaving disks in an unexpected state.

The same bug exists in all 8 test files that use initrd LUKS fixtures, and the fixture boilerplate is heavily duplicated. Fix both: extract a shared fixture module and fix the dm-crypt availability in one place.

## Root Cause (verified from `nix log`)

The initrd fixtures declare `boot.initrd.kernelModules = [ "dm-crypt" ]`, which tells systemd to **load** the module — but does NOT ensure the module binary is **included** in the initrd filesystem. On the macOS linux builder (aarch64), dm-crypt is likely built-in. On native x86_64 NixOS, it's a loadable module that must be explicitly included.

Evidence from the full VM log:

```
systemd-modules-load[72]: Failed to insert module 'dm_crypt': No such file or directory
device-mapper: table: 254:0: crypt: unknown target type
prepare-luks-btrfs-fixture-start[204]: device-mapper: reload ioctl on braid-disk1-fmt (254:0) failed: Invalid argument
prepare-luks-btrfs-fixture.service: Main process exited, code=exited, status=5/NOTINSTALLED
Failed to start Prepare LUKS + btrfs RAID1 fixture with bricked disk3.
```

Failure sequence:

1. `cryptsetup luksFormat` succeeds (raw I/O — no kernel module needed)
2. `cryptsetup luksOpen` fails (needs dm-crypt target — module not in initrd)
3. `set -eu` exits the script — no btrfs created, no disk bricked
4. Service is `wantedBy` (soft dep), boot continues
5. `braid unlock` in stage 2 (dm-crypt available) opens all 3 LUKS devices
6. Mount fails: "wrong fs type" — no btrfs filesystem inside the LUKS containers

## Affected Files

All 8 test files with initrd LUKS fixtures:

- `tests/module/single-disk.nix`
- `tests/module/single-disk-dead.nix`
- `tests/module/raid1.nix`
- `tests/module/degraded-raid1.nix`
- `tests/module/no-silent-degraded.nix`
- `tests/module/auto-unlock-key-present.nix`
- `tests/module/auto-unlock-key-missing.nix`
- `tests/module/auto-unlock-key-wrong.nix`

## Plan

### Step 1: Create `tests/module/lib/initrd-fixture.nix`

A function that takes parameters and returns a `boot.initrd` attrset fragment. Centralizes:

- `availableKernelModules = [ "dm-crypt" ]` (the bug fix)
- `kernelModules = [ "dm-crypt" ]`
- `systemd.enable = true`
- `storePaths` (cryptsetup, btrfs-progs, util-linux + extras)
- The oneshot service config (wantedBy/before/after/DefaultDependencies)
- Device wait loop, LUKS format loop, LUKS open with `-fmt` suffix
- mkfs.btrfs (auto-selects single vs raid1 based on disk count)
- LUKS close loop

Interface:

```nix
{
  pkgs, lib,
  passphrase,                 # LUKS passphrase
  diskNames,                  # [ "disk1" "disk2" ... ]
  extraWaitDevices ? [],      # extra block devices to wait for (e.g., USB)
  extraStorePaths ? [],       # extra packages in initrd store
  extraPath ? [],             # extra packages on service PATH
  supportedFilesystems ? [],  # e.g., [ "btrfs" ] for in-initrd mount
  preCloseScript ? "",        # runs after mkfs, before luksClose (mappers open)
  postScript ? "",            # runs after luksClose (mappers closed)
  description ? "Prepare LUKS + btrfs fixture",
}
```

### Step 2: Update each test file

Replace inline `boot.initrd` blocks with `boot.initrd = import ./lib/initrd-fixture.nix { ... };`

| Test                    | diskNames                     | extras                                                                                                                                |
| ----------------------- | ----------------------------- | ------------------------------------------------------------------------------------------------------------------------------------- |
| single-disk             | `[ "disk1" ]`                 | none                                                                                                                                  |
| single-disk-dead        | `[ "disk1" ]`                 | `postScript`: dd brick disk1                                                                                                          |
| raid1                   | `[ "disk1" "disk2" "disk3" ]` | none                                                                                                                                  |
| degraded-raid1          | `[ "disk1" "disk2" "disk3" ]` | `supportedFilesystems = [ "btrfs" ]`, `preCloseScript`: mount + write test data, `postScript`: dd brick disk3                         |
| no-silent-degraded      | `[ "disk1" "disk2" "disk3" ]` | identical to degraded-raid1                                                                                                           |
| auto-unlock-key-missing | `[ "disk1" ]`                 | none                                                                                                                                  |
| auto-unlock-key-wrong   | `[ "disk1" ]`                 | `extraWaitDevices`: USB, `extraStorePaths`/`extraPath`: e2fsprogs, `postScript`: format USB + write random keyfile (not enrolled)     |
| auto-unlock-key-present | `[ "disk1" "disk2" ]`         | `extraWaitDevices`: USB, `extraStorePaths`/`extraPath`: e2fsprogs, `postScript`: format USB + write keyfile + enroll via `luksAddKey` |

Note on auto-unlock-key-present ordering: the current fixture does USB formatting + key enrollment **before** luksOpen. The refactored version moves it to `postScript` (after luksClose). This is safe because `cryptsetup luksAddKey` operates on the raw LUKS device, not the mapper.

### Scope

Test-only. No changes to:

- braid NixOS module (`modules/braid/`)
- CLI code (`cli/src/`)
- User-facing config or README

## Verification

```
just test
```

All tests must pass, not just the two currently failing. In particular, all 8 tests with initrd fixtures should be verified working on the native NixOS x86_64 runner.
