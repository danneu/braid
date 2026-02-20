#!/usr/bin/env bash
# braid-remove-disk — Remove a disk from the braid pool.
# Usage: sudo braid-remove-disk /dev/disk/by-id/<device>

# --- Read config ---

CONFIG_FILE="/etc/braid/config.json"
if [[ "${1:-}" == "--config" ]]; then
  CONFIG_FILE="$2"; shift 2
fi

if [[ ! -f "$CONFIG_FILE" ]]; then
  echo "Error: $CONFIG_FILE not found."
  echo "Is the braid module enabled? Check your NixOS config."
  exit 1
fi

MOUNT_POINT=$(jq -r '.mountPoint' "$CONFIG_FILE")

# --- No-args listing ---

if [[ $# -eq 0 ]]; then
  echo ""
  echo "Removable disks (in pool but not in config):"
  echo ""

  if ! mountpoint -q "$MOUNT_POINT" 2>/dev/null; then
    echo "  Pool is not mounted at $MOUNT_POINT"
    echo ""
    exit 0
  fi

  configured_disks=$(jq -r '.disks[]' "$CONFIG_FILE" 2>/dev/null || true)
  pool_mappers=$(btrfs filesystem show "$MOUNT_POINT" 2>/dev/null | grep -oP '/dev/mapper/\S+' || true)

  found=false
  if [[ -n "$pool_mappers" ]]; then
    while IFS= read -r mapper_path; do
      mapper_name=$(basename "$mapper_path")
      by_id="/dev/disk/by-id/$mapper_name"

      # Skip if still in config
      if [[ -n "$configured_disks" ]] && echo "$configured_disks" | grep -qxF "$by_id"; then
        continue
      fi

      real=$(readlink -f "$by_id" 2>/dev/null || true)
      model=$(lsblk -ndo MODEL "$real" 2>/dev/null | xargs || true)
      size=$(lsblk -ndo SIZE "$real" 2>/dev/null | xargs || true)

      printf "  %-55s %-20s %s\n" "$by_id" "${model:-(unknown)}" "${size}"
      found=true
    done <<< "$pool_mappers"
  fi

  # Check for missing devices
  if btrfs filesystem show "$MOUNT_POINT" 2>/dev/null | grep -qi "missing"; then
    echo "  (missing device detected in pool)"
    found=true
  fi

  if ! $found; then
    echo "  (none — all pool members are in config)"
  fi

  echo ""
  echo "Usage: braid-remove-disk <disk>"
  exit 0
fi

# --- Validate arguments ---

if [[ $# -ne 1 ]]; then
  echo "Usage: braid-remove-disk <block-device>"
  echo "Example: braid-remove-disk /dev/disk/by-id/ata-Toshiba_MN07_XXXX"
  exit 1
fi

disk="$1"

# Require by-id paths
if [[ "$disk" != /dev/disk/by-id/* ]]; then
  echo "Error: Use a stable disk path (/dev/disk/by-id/...)"
  echo "Find yours with: ls /dev/disk/by-id/"
  exit 1
fi

# --- Inverse config guard: disk must NOT be in braid.disks ---

if jq -e --arg disk "$disk" '.disks | index($disk)' "$CONFIG_FILE" >/dev/null 2>&1; then
  remaining_disks=$(jq -r '.disks[] | select(. != $disk)' --arg disk "$disk" "$CONFIG_FILE" 2>/dev/null || true)
  echo "Error: $disk is still in braid.disks."
  echo ""
  echo "Remove it from your NixOS config:"
  echo ""
  echo "  braid.disks = ["
  if [[ -n "$remaining_disks" ]]; then
    while IFS= read -r d; do
      echo "    \"$d\""
    done <<< "$remaining_disks"
  fi
  echo "  ];"
  echo ""
  echo "Then run: sudo nixos-rebuild switch"
  echo "Then run: sudo braid-remove-disk $disk"
  exit 1
fi

# --- Pool must be mounted ---

if ! mountpoint -q "$MOUNT_POINT" 2>/dev/null; then
  echo "Error: Pool is not mounted at $MOUNT_POINT"
  echo "The pool must be mounted to remove a disk."
  exit 1
fi

# --- Three-tier detection ---

mapper_name=$(basename "$disk")
mapper_path="/dev/mapper/$mapper_name"

tier=""

if [[ -e "$mapper_path" ]] && cryptsetup status "$mapper_name" >/dev/null 2>&1; then
  # Tier 1: mapper exists and is open — verify it's in the pool
  pool_devs=$(btrfs filesystem show "$MOUNT_POINT" 2>/dev/null || true)
  if echo "$pool_devs" | grep -q "$mapper_path"; then
    tier="graceful"
  else
    echo "Error: $mapper_path is open but not part of the btrfs pool at $MOUNT_POINT."
    echo "Cannot remove a device that isn't in the pool."
    exit 1
  fi
elif btrfs filesystem show "$MOUNT_POINT" 2>/dev/null | grep -qi "missing"; then
  # Tier 2: mapper doesn't exist, pool has a missing device
  missing_count=$(btrfs filesystem show "$MOUNT_POINT" 2>/dev/null | grep -ci "missing" || echo "0")
  if [[ "$missing_count" -gt 1 ]]; then
    echo "Error: Multiple missing devices detected ($missing_count)."
    echo "Cannot determine which device to remove."
    echo ""
    echo "Resolve manually:"
    echo "  btrfs device remove missing $MOUNT_POINT"
    exit 1
  fi
  tier="missing"
else
  # Tier 3: neither open nor missing
  echo "Error: $disk is not currently open and no missing devices detected in the pool."
  echo ""
  echo "Possible causes:"
  echo "  - The disk was already removed"
  echo "  - The disk was never part of this pool"
  echo "  - The system was rebooted and the disk wasn't unlocked (check pool status)"
  echo ""
  echo "Check pool status: btrfs filesystem show $MOUNT_POINT"
  exit 1
fi

# --- Count remaining disks ---

device_count=$(btrfs filesystem show "$MOUNT_POINT" 2>/dev/null | grep -c "devid" || echo "0")
remaining_after=$((device_count - 1))

# --- Disk info + confirmation ---

echo ""
if [[ "$tier" == "graceful" ]]; then
  real_disk=$(readlink -f "$disk" 2>/dev/null || true)
  disk_model=$(lsblk -ndo MODEL "$real_disk" 2>/dev/null | xargs || true)
  disk_size=$(lsblk -ndo SIZE "$real_disk" 2>/dev/null | xargs || true)
  disk_serial=$(lsblk -ndo SERIAL "$real_disk" 2>/dev/null | xargs || true)

  echo "Removing from pool:"
  echo "  $disk"
  echo "  Model:  ${disk_model:-(unknown)}"
  echo "  Size:   $disk_size"
  echo "  Serial: ${disk_serial:-(unknown)}"
else
  echo "Removing missing device from pool."
  echo "  Original path: $disk (not present)"
fi

echo ""
echo "Pool: $device_count device(s) -> $remaining_after device(s)"
echo ""

if [[ "$remaining_after" -lt 2 ]]; then
  echo "WARNING: This leaves $remaining_after disk with no RAID1 redundancy."
  echo "A single disk failure will cause data loss."
  echo ""
  echo "Type 'remove this disk without redundancy' to confirm:"

  read -r confirmation
  if [[ "$confirmation" != "remove this disk without redundancy" ]]; then
    echo "Aborted."
    exit 1
  fi
else
  echo "Type 'remove this disk' to confirm:"

  read -r confirmation
  if [[ "$confirmation" != "remove this disk" ]]; then
    echo "Aborted."
    exit 1
  fi
fi

# --- Convert profile if dropping below RAID1 minimum ---

if [[ "$remaining_after" -lt 2 ]]; then
  echo "Converting pool from RAID1 to single profile..."
  btrfs balance start -dconvert=single -mconvert=single -f "$MOUNT_POINT"
fi

# --- Execute remove ---

if [[ "$tier" == "graceful" ]]; then
  echo "Removing $mapper_path from btrfs pool (migrating data off)..."
  btrfs device remove "$mapper_path" "$MOUNT_POINT"
else
  echo "Removing missing device from btrfs pool..."
  btrfs device remove missing "$MOUNT_POINT"
fi

# --- LUKS cleanup (Tier 1 only) ---

if [[ "$tier" == "graceful" ]]; then
  echo "Closing LUKS device $mapper_name..."
  if cryptsetup close "$mapper_name"; then
    echo "Disk fully released — safe to physically remove."
  else
    echo ""
    echo "Error: Could not close LUKS device $mapper_name."
    echo "The btrfs remove succeeded, but the LUKS mapper is still open."
    echo ""
    echo "Check what's using it:"
    echo "  fuser -vm /dev/mapper/$mapper_name"
    echo ""
    echo "Then close it:"
    echo "  cryptsetup close $mapper_name"
    echo ""
    echo "The mapper will close automatically on reboot."
    exit 1
  fi
fi

# --- Summary ---

device_count=$(btrfs filesystem show "$MOUNT_POINT" 2>/dev/null | grep -c "devid" || echo "?")
profile=$(btrfs filesystem df "$MOUNT_POINT" 2>/dev/null | head -1 || echo "unknown")

echo ""
echo "Done."
echo ""
echo "Pool status: $device_count device(s), $profile"

if [[ "$device_count" -eq 1 ]]; then
  echo ""
  echo "WARNING: Pool has only 1 disk — no RAID1 redundancy."
  echo "Add a second disk with braid-add-disk to restore protection."
fi
