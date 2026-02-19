#!/usr/bin/env bash
# btrnas-add-disk — Format a disk with LUKS and add it to the btrnas pool.
# Usage: sudo btrnas-add-disk /dev/disk/by-id/<device>

MOUNT_POINT="/mnt/storage"

# --- Step 1: Validate arguments ---

if [[ $# -eq 0 ]]; then
  # Find root disk to exclude
  root_dev=$(findmnt -n -o SOURCE / 2>/dev/null | head -1)
  root_disk=""
  if [[ -n "$root_dev" ]]; then
    root_disk=$(lsblk -ndo PKNAME "$root_dev" 2>/dev/null || true)
    [[ -z "$root_disk" ]] && root_disk=$(basename "$root_dev")
  fi

  # Collect pool member mapper paths to exclude
  pool_mappers=""
  if mountpoint -q "$MOUNT_POINT" 2>/dev/null; then
    pool_mappers=$(btrfs filesystem show "$MOUNT_POINT" 2>/dev/null | grep -oP '/dev/mapper/\S+' || true)
  fi

  echo ""
  echo "Available disks (not in pool):"
  echo ""

  found=false
  for by_id in /dev/disk/by-id/*; do
    [[ "$by_id" == *-part* ]] && continue
    [[ ! -b "$by_id" ]] && continue

    real=$(readlink -f "$by_id")
    base=$(basename "$real")

    # Skip root disk
    [[ "$base" == "$root_disk" ]] && continue

    # Skip if already open and in pool
    mapper="btrnas-$base"
    if [[ -n "$pool_mappers" ]] && echo "$pool_mappers" | grep -q "/dev/mapper/$mapper"; then
      continue
    fi

    model=$(lsblk -ndo MODEL "$real" 2>/dev/null | xargs || true)
    size=$(lsblk -ndo SIZE "$real" 2>/dev/null | xargs || true)

    printf "  %-55s %-20s %s\n" "$by_id" "${model:-(unknown)}" "${size}"
    found=true
  done

  if ! $found; then
    echo "  (none found)"
  fi

  echo ""
  echo "Usage: btrnas-add-disk <disk>"
  exit 0
fi

if [[ $# -ne 1 ]]; then
  echo "Usage: btrnas-add-disk <block-device>"
  echo "Example: btrnas-add-disk /dev/disk/by-id/ata-Toshiba_MN07_XXXX"
  exit 1
fi

disk="$1"

if [[ ! -b "$disk" ]]; then
  echo "Error: $disk is not a block device"
  exit 1
fi

# Resolve the real device path for mapper naming
real_disk=$(readlink -f "$disk")
mapper_name="btrnas-$(basename "$real_disk")"

# --- Step 2: Check disk state (safety guards) ---

if cryptsetup isLuks "$disk" 2>/dev/null; then
  # First check if the device is already opened as a dm-crypt mapper
  existing_mapper=$(lsblk -nlo TYPE,NAME "$real_disk" | awk '$1 == "crypt" {print $2}' | head -1)

  if [[ -n "$existing_mapper" ]]; then
    # Device is already open — check if it's part of our pool
    if mountpoint -q "$MOUNT_POINT" 2>/dev/null; then
      pool_devs=$(btrfs filesystem show "$MOUNT_POINT" 2>/dev/null || true)
      if echo "$pool_devs" | grep -q "/dev/mapper/$existing_mapper"; then
        echo "Error: $disk is already in the btrnas pool (as /dev/mapper/$existing_mapper)."
        exit 1
      fi
    fi
    echo "Error: $disk is LUKS-formatted and currently open as /dev/mapper/$existing_mapper."
    echo "Close it first or wipe it: wipefs -a $disk"
    exit 1
  fi

  # Device is LUKS but not open — try to probe if passphrase is available
  if [[ -z "${BTRNAS_PASSPHRASE:-}" ]]; then
    echo "Error: $disk is already LUKS-formatted."
    echo "If you want to re-use it, wipe it first: wipefs -a $disk"
    exit 1
  fi

  tmp_mapper="btrnas-probe-$$"
  if ! echo -n "$BTRNAS_PASSPHRASE" | cryptsetup luksOpen --key-file=- "$disk" "$tmp_mapper" 2>/dev/null; then
    echo "Error: $disk is LUKS-formatted but the passphrase doesn't match."
    echo "If you want to re-use it, wipe it first: wipefs -a $disk"
    exit 1
  fi

  # Check what's inside the LUKS container
  inner_type=$(blkid -o value -s TYPE "/dev/mapper/$tmp_mapper" 2>/dev/null || true)

  if [[ "$inner_type" == "btrfs" ]]; then
    # LUKS + btrfs but not in our pool (would have been caught above if open)
    cryptsetup luksClose "$tmp_mapper"
    echo "Error: $disk is LUKS-formatted and contains a btrfs filesystem."
    echo "If you want to re-use it, wipe it first: wipefs -a $disk"
    exit 1
  elif [[ -z "$inner_type" ]]; then
    # LUKS but no filesystem inside — crash recovery case
    cryptsetup luksClose "$tmp_mapper"
    echo "Warning: $disk is LUKS-formatted but contains no filesystem."
    echo "This may be from a previous interrupted btrnas-add-disk run."
    echo "The disk will be wiped and re-formatted."
    echo ""
    # Fall through to normal formatting path — wipefs first
    wipefs -a "$disk"
  else
    # LUKS + some other filesystem
    cryptsetup luksClose "$tmp_mapper"
    echo "Error: $disk is LUKS-formatted and contains a $inner_type filesystem."
    echo "If you want to re-use it, wipe it first: wipefs -a $disk"
    exit 1
  fi
fi

# --- Step 3: Detect pool state ---

pool_exists=false
if mountpoint -q "$MOUNT_POINT" 2>/dev/null; then
  mount_type=$(findmnt -n -o FSTYPE "$MOUNT_POINT" 2>/dev/null || true)
  if [[ "$mount_type" == "btrfs" ]]; then
    pool_exists=true
  fi
else
  # Pool not mounted — check if there are other LUKS devices (existing pool, just not unlocked)
  other_luks=$(blkid -t TYPE=crypto_LUKS -o device 2>/dev/null || true)
  # Filter out the disk we're about to format
  other_luks=$(echo "$other_luks" | grep -v "^$(readlink -f "$disk")$" || true)
  if [[ -n "$other_luks" ]]; then
    echo "Error: Found LUKS-encrypted devices on this system that are not unlocked:"
    echo "$other_luks"
    echo ""
    echo "This likely means an existing btrnas pool exists but hasn't been unlocked yet."
    echo "Unlock your existing pool first, then run btrnas-add-disk again."
    exit 1
  fi
fi

# --- Step 4: Print disk info + confirmation ---

disk_model=$(lsblk -ndo MODEL "$real_disk" 2>/dev/null | xargs || true)
disk_size=$(lsblk -ndo SIZE "$real_disk" 2>/dev/null | xargs || true)
disk_serial=$(lsblk -ndo SERIAL "$real_disk" 2>/dev/null | xargs || true)

if [[ -z "$disk_model" ]]; then
  disk_model="(unknown)"
fi

echo ""
echo "WARNING: This will PERMANENTLY ERASE all data on:"
echo "  $disk"
echo "  Model:  $disk_model"
echo "  Size:   $disk_size"
echo "  Serial: ${disk_serial:-(unknown)}"
echo ""

if $pool_exists; then
  echo "It will be LUKS-encrypted and added to the btrfs pool at $MOUNT_POINT."
else
  echo "It will be LUKS-encrypted and become the first disk in a new btrfs pool."
  echo "NOTE: A single disk has NO redundancy. Add a second disk for RAID1 protection."
fi

echo ""
echo "Type 'erase this disk' to confirm:"

read -r confirmation
if [[ "$confirmation" != "erase this disk" ]]; then
  echo "Aborted."
  exit 1
fi

# --- Step 5: Get passphrase ---

passphrase=""

if [[ -n "${BTRNAS_PASSPHRASE:-}" ]]; then
  passphrase="$BTRNAS_PASSPHRASE"
elif $pool_exists; then
  # Prompt once, verify against existing LUKS device
  read -r -s -p "Enter LUKS passphrase (must match existing disks): " passphrase
  echo ""

  # Find an existing LUKS device in the pool to verify against
  pool_dev=$(btrfs filesystem show "$MOUNT_POINT" 2>/dev/null | grep -oP '/dev/mapper/\S+' | head -1)
  if [[ -z "$pool_dev" ]]; then
    echo "Error: Could not find an existing device in the pool to verify passphrase."
    exit 1
  fi

  # Find the underlying LUKS device for this mapper
  luks_dev=$(cryptsetup status "$pool_dev" 2>/dev/null | grep "device:" | awk '{print $2}')
  if [[ -z "$luks_dev" ]]; then
    echo "Error: Could not determine LUKS device for $pool_dev"
    exit 1
  fi

  if ! echo -n "$passphrase" | cryptsetup luksOpen --test-passphrase --key-file=- "$luks_dev" 2>/dev/null; then
    echo "Error: Passphrase doesn't match your existing disks."
    exit 1
  fi
else
  # First disk — prompt twice, confirm match
  read -r -s -p "Enter new LUKS passphrase: " passphrase
  echo ""
  read -r -s -p "Confirm LUKS passphrase: " passphrase2
  echo ""
  if [[ "$passphrase" != "$passphrase2" ]]; then
    echo "Error: Passphrases don't match."
    exit 1
  fi
fi

if [[ -z "$passphrase" ]]; then
  echo "Error: Passphrase cannot be empty."
  exit 1
fi

# --- Step 6: LUKS format + open ---

# Parse BTRNAS_LUKS_OPTS into array (shellcheck-clean)
luks_extra_opts=()
if [[ -n "${BTRNAS_LUKS_OPTS:-}" ]]; then
  read -ra luks_extra_opts <<< "$BTRNAS_LUKS_OPTS"
fi

echo "Formatting $disk with LUKS..."
echo -n "$passphrase" | cryptsetup luksFormat --batch-mode --key-file=- "${luks_extra_opts[@]}" "$disk"

echo "Opening LUKS device as $mapper_name..."
echo -n "$passphrase" | cryptsetup luksOpen --key-file=- "$disk" "$mapper_name"

# Cleanup trap: close LUKS on failure
cleanup() {
  if [[ -e "/dev/mapper/$mapper_name" ]]; then
    cryptsetup luksClose "$mapper_name" 2>/dev/null || true
  fi
}
trap cleanup ERR

# --- Step 7: btrfs ---

if $pool_exists; then
  echo "Adding /dev/mapper/$mapper_name to btrfs pool..."
  btrfs device add "/dev/mapper/$mapper_name" "$MOUNT_POINT"
  echo "Starting RAID1 balance (this may take a while on large pools)..."
  btrfs balance start -dconvert=raid1 -mconvert=raid1 "$MOUNT_POINT"
  # Evict any dead devices left in the pool (e.g. replacing a failed drive)
  if btrfs filesystem show "$MOUNT_POINT" 2>/dev/null | grep -qi "missing"; then
    echo "Removing missing (dead) device from pool..."
    btrfs device remove missing "$MOUNT_POINT"
  fi
else
  echo "Creating btrfs filesystem on /dev/mapper/$mapper_name..."
  mkfs.btrfs -f "/dev/mapper/$mapper_name"
  mkdir -p "$MOUNT_POINT"
  mount "/dev/mapper/$mapper_name" "$MOUNT_POINT"
fi

# Clear the ERR trap — we succeeded
trap - ERR

# --- Step 8: Print next steps ---

luks_uuid=$(cryptsetup luksUUID "$disk")
device_count=$(btrfs filesystem show "$MOUNT_POINT" 2>/dev/null | grep -c "devid" || echo "?")
profile=$(btrfs filesystem df "$MOUNT_POINT" 2>/dev/null | head -1 || echo "unknown")

echo ""
echo "Done."
echo ""
echo "Add this disk to your NixOS config:"
echo ""
echo "  btrnas.disks = ["
echo "    \"$disk\"   # ← new"
echo "  ];"
echo ""
echo "Then run: sudo nixos-rebuild switch"
echo ""
echo "LUKS UUID: $luks_uuid"
echo "Pool status: $device_count device(s), $profile"
