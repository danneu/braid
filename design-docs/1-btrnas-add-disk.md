# Design: btrnas-add-disk

## Purpose

One-shot imperative CLI tool that formats a new physical disk and adds it to the btrnas pool. This is the only destructive action in the system — `nixos-rebuild switch` never formats or destroys data.

## Usage

```
$ sudo btrnas-add-disk /dev/disk/by-id/ata-Toshiba_MN07_XXXX
```

## Behavior by pool state

### No pool exists yet (first disk)

1. Validate the block device exists and is not already LUKS
2. Print disk info (model, size, serial) via `lsblk`/`udevadm`
3. Prompt for LUKS passphrase twice (confirm match)
4. `cryptsetup luksFormat` the device
5. `cryptsetup luksOpen` with a temp mapper name
6. `mkfs.btrfs` (single device, no RAID1 — can't with 1 drive)
7. Mount at `/mnt/storage`
8. Print the by-id path and tell the user to add it to `btrnas.disks`
9. Warn: "No redundancy yet — add a second disk for RAID1 protection."

### Pool exists with 1 device (second disk → enables RAID1)

1. Validate the block device exists and is not already LUKS
2. Print disk info
3. Prompt for LUKS passphrase once
4. **Verify passphrase** by opening an existing LUKS device from the pool — if it fails, refuse to proceed ("Passphrase doesn't match your existing disks")
5. `cryptsetup luksFormat` the new device with the verified passphrase
6. `cryptsetup luksOpen` with a temp mapper name
7. `btrfs device add /dev/mapper/<temp> /mnt/storage`
8. `btrfs balance start -dconvert=raid1 -mconvert=raid1 /mnt/storage` (background)
9. Print the by-id path and tell the user to add it to `btrnas.disks`
10. "Pool now has 2 disks with RAID1 redundancy. Balance is running in background."

### Pool exists with 2+ devices (third+ disk)

Same as above. `btrfs device add` + `balance -dconvert=raid1` to redistribute across all devices.

## Design decisions

### Passphrase handling

- **First disk:** prompt twice, confirm match. Standard new-passphrase flow.
- **Subsequent disks:** prompt once, then verify against an existing LUKS device in the pool. This enforces all disks use the same passphrase without storing it anywhere. The remote unlock UX depends on this — one passphrase unlocks all drives.
- **How verification works:** the script reads which devices are in the btrfs pool (`btrfs fi show /mnt/storage`), picks one, tries `cryptsetup luksOpen --test-passphrase` against it. If it fails, the user mistyped or used a different passphrase.

### LUKS mapper naming

- The script uses a **temp mapper name** during its one-shot operation (e.g. `btrnas-tmp`). It closes it when done if needed, or leaves it open for immediate use.
- The **NixOS module** derives mapper names from the by-id path at boot. `/dev/disk/by-id/ata-Toshiba_MN07_XXXX` → mapper name `ata-Toshiba_MN07_XXXX`. This is deterministic and requires no coordination — both script and module derive from the same source.
- The mapper name doesn't matter for btrfs — it finds all devices by UUID internally. The naming is only for LUKS open/close.

### Pool detection

- If `/mnt/storage` is a mounted btrfs filesystem: pool exists. `btrfs fi show /mnt/storage` gives device count.
- If `/mnt/storage` is not mounted: no pool yet (first disk path).
- No config parsing. The script reads live system state only.

### Safety checks

The script must refuse to proceed if:
- The device is already LUKS-formatted (`cryptsetup isLuks`)
- The device has a recognized filesystem (`blkid`)
- The device is currently mounted
- The user doesn't type the exact confirmation phrase

### Confirmation UX

```
WARNING: This will PERMANENTLY ERASE all data on:
  /dev/disk/by-id/ata-Toshiba_MN07_XXXX
  Model: Toshiba MN07ACA12T
  Size:  12 TB
  Serial: XXXXXXXXXXXX

It will be LUKS-encrypted and added to the btrfs pool at /mnt/storage.

Type 'erase this disk' to confirm:
```

### Output on completion

```
Done.

Add this disk to your NixOS config:

  btrnas.disks = [
    "/dev/disk/by-id/ata-Toshiba_MN07_XXXX"   # existing
    "/dev/disk/by-id/ata-Ironwolf_ST12_YYYY"   # ← new
  ];

Then run: sudo nixos-rebuild switch

LUKS UUID: xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx
Pool status: 2 devices, RAID1, balance running
```

## Implementation

Bash script, packaged via `pkgs.writeShellApplication`:

```nix
btrnas-add-disk = pkgs.writeShellApplication {
  name = "btrnas-add-disk";
  runtimeInputs = [ pkgs.cryptsetup pkgs.btrfs-progs pkgs.util-linux ];
  text = builtins.readFile ./btrnas-add-disk.sh;
};
```

Estimated ~100 lines of bash. Sequential CLI calls with conditionals, no concurrency, no complex data structures.

### Dependencies in PATH

- `cryptsetup` — LUKS format/open/test-passphrase
- `btrfs` — mkfs, device add, balance, fi show
- `lsblk` — disk info display
- `udevadm` — model/serial lookup
- `blkid` — existing filesystem detection

## Relationship to the NixOS module

The script and module are independent:

- **Script** reads live system state, runs destructive one-shot operations
- **Module** reads the NixOS config, sets up boot-time unlock/mount/samba

They agree on:
- Mount point: `/mnt/storage`
- Disk identification: `/dev/disk/by-id/` paths
- LUKS mapper naming: derived from by-id path

The script's final output tells the user exactly what to put in the module config. The module never calls the script.

## Testing

The script can be tested in NixOS VM tests with virtual drives, same as the existing test suite. A dedicated `btrnas-add-disk` test would:
1. Boot a VM with 3 empty virtual drives
2. Run `btrnas-add-disk` on the first drive (first-disk path)
3. Verify btrfs single-device pool at `/mnt/storage`
4. Run `btrnas-add-disk` on the second drive (add-to-pool path)
5. Verify btrfs RAID1 with 2 devices, balance started
6. Run `btrnas-add-disk` on the third drive
7. Verify 3 devices in pool

This replaces the manual LUKS/btrfs setup that every test currently does imperatively.
