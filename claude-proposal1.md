# Proposal: Separate disk initialization from pool reconciliation

**Status:** Draft

## Problem

`braid apply` currently handles both LUKS formatting (destructive, irreversible) and pool reconciliation (safe, repeatable) in a single command. The planner can't distinguish between "a configured disk that fell out of the pool" and "a brand-new disk that needs formatting." This means:

1. Disk gets unplugged accidentally
2. `braid apply` sees "disk in config, not in pool" → plans `luksFormat`
3. Disk gets plugged back in → `luksFormat` destroys all data

The root cause isn't a missing safety check — it's that a destructive one-time action lives inside a reconciliation loop designed to be safe and repeatable.

## Proposal

Split into two commands with different safety properties:

| Command | Purpose | Destructive? | Run frequency |
|---|---|---|---|
| `braid init-disk` | One-time LUKS formatting of a new disk | Yes | Once per disk lifetime |
| `braid apply` | Pool reconciliation: open, add, remove, balance | No | Unlimited, always safe |

`braid apply` **never calls `cryptsetup luksFormat`**. It only calls `cryptsetup luksOpen` (non-destructive). If a disk isn't LUKS-formatted, apply reports an error and tells the user to run `braid init-disk`.

## Design: `braid init-disk`

```
braid init-disk /dev/disk/by-id/<disk-id>
```

Behavior:
1. Validate device exists and is a block device
2. Check device is NOT already LUKS-formatted (`cryptsetup isLuks`). Refuse if it is (require `--force` to override).
3. Check device is not currently in the btrfs pool. Refuse if it is.
4. Read passphrase from `BRAID_PASSPHRASE` env var (or prompt interactively in the future)
5. `cryptsetup luksFormat` with standard parameters
6. Print success message. Do NOT open the device or add to pool — that's `apply`'s job.

The `--force` flag exists for the rare case where you genuinely want to reformat a disk (e.g., repurposing a disk from another system). It requires the device to NOT be in the active pool.

## Design: `braid apply` (revised)

The planner changes:

**Current behavior** when a config disk is not in the pool:
```
→ ADD_DISK_LUKS_FORMAT_OPEN   (destructive!)
→ ADD_DISK_BTRFS_ADD
```

**New behavior** when a config disk is not in the pool:
- Device present → `OPEN_LUKS` + `ADD_DISK_BTRFS_ADD`
- Device present but not LUKS → error: "disk not initialized, run `braid init-disk` first"
- Device absent → warning: "device not found, skipping" (no actions generated for this disk)

The `OPEN_LUKS` action calls `cryptsetup luksOpen`, which is non-destructive. If the passphrase is wrong, it fails cleanly.

## Workflows

### First disk (bootstrap)

```bash
# 1. Initialize the disk
sudo BRAID_PASSPHRASE="..." braid init-disk /dev/disk/by-id/ata-disk1

# 2. Config already lists the disk, so just apply
sudo BRAID_PASSPHRASE="..." braid apply
# → opens LUKS, creates btrfs, mounts
```

### Add a second disk

```bash
# 1. Initialize
sudo BRAID_PASSPHRASE="..." braid init-disk /dev/disk/by-id/ata-disk2

# 2. Add to NixOS config, rebuild
# braid.disks = [ "..." "/dev/disk/by-id/ata-disk2" ];

# 3. Apply
sudo BRAID_PASSPHRASE="..." braid apply
# → opens LUKS, adds to pool, balances to RAID1
```

### Disk falls out and comes back

```bash
# Disk unplugged — apply is safe
sudo braid apply
# → "device ata-disk1 not found, skipping" (no damage)

# Plug disk back in — apply recovers
sudo BRAID_PASSPHRASE="..." braid apply
# → luksOpen, re-add to pool. Data intact.
```

### Replace a failed disk

```bash
# 1. Remove old disk from NixOS config, rebuild
# 2. Initialize new disk
sudo BRAID_PASSPHRASE="..." braid init-disk /dev/disk/by-id/ata-new-disk

# 3. Add new disk to config, rebuild
# 4. Apply
sudo BRAID_PASSPHRASE="..." braid apply
# → removes old/missing, opens new, adds to pool, balances
```

## What changes

- **`braid.sh`**: Remove `action_luks_format_open`. Add `OPEN_LUKS` action type (just `luksOpen`). Planner generates errors/warnings for uninitialized or absent devices instead of format actions.
- **`braid.sh`**: Add `cmd_init_disk` subcommand and `init-disk` to the dispatcher.
- **`cli.nix`**: No changes needed (braid already has cryptsetup in runtimeInputs).
- **Tests**: Update existing plan/apply tests. Add new tests for `braid init-disk` and for the "apply refuses to format" behavior.
- **Standalone scripts**: `braid-add-disk.sh` also auto-formats. It should either be updated to match, or deprecated in favor of the `braid init-disk` + `braid apply` workflow.

## What doesn't change

- Config format (`braid.disks` stays as a list of by-id paths)
- `braid plan` / `braid apply` / `braid status` subcommand structure
- Checkpoint/resume system
- `braid-remove-disk`, `braid-status` standalone scripts

## Why this is the right fix

The alternative — probing for LUKS headers at plan time — works but papers over the design issue. You're still one missed edge case away from `luksFormat` running on a disk with data. Removing `luksFormat` from the reconciliation loop eliminates the entire category of bug. There's no edge case to miss because the dangerous operation doesn't exist in that code path.

This mirrors how NixOS itself works: `nixos-install` is the one-time destructive setup; `nixos-rebuild` is the safe, repeatable reconciliation. Nobody expects `nixos-rebuild` to partition their drives.
