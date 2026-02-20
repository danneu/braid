#!/usr/bin/env bash
# braid-status — Show pool health and per-disk detail.
# Usage: braid-status [--verbose] [--config <path>]

# --- Parse arguments ---

VERBOSE=false
CONFIG_FILE="/etc/braid/config.json"
while [[ $# -gt 0 ]]; do
  case "$1" in
    --verbose) VERBOSE=true; shift ;;
    --config) CONFIG_FILE="$2"; shift 2 ;;
    *) echo "Usage: braid-status [--verbose]"; exit 1 ;;
  esac
done

# --- Read config ---

if [[ ! -f "$CONFIG_FILE" ]]; then
  echo "Error: $CONFIG_FILE not found."
  echo "Is the braid module enabled? Check your NixOS config."
  exit 1
fi

MOUNT_POINT=$(jq -r '.mountPoint' "$CONFIG_FILE")

# --- Validate pool is mounted ---

if ! mountpoint -q "$MOUNT_POINT" 2>/dev/null; then
  echo "Error: $MOUNT_POINT is not mounted."
  exit 1
fi

mount_type=$(findmnt -n -o FSTYPE "$MOUNT_POINT" 2>/dev/null || true)
if [[ "$mount_type" != "btrfs" ]]; then
  echo "Error: $MOUNT_POINT is not a btrfs filesystem."
  exit 1
fi

# --- Gather btrfs data ---

fi_show=$(btrfs filesystem show "$MOUNT_POINT")
fi_df=$(btrfs filesystem df "$MOUNT_POINT")
fi_usage=$(btrfs filesystem usage --raw "$MOUNT_POINT")
scrub_output=$(btrfs scrub status "$MOUNT_POINT" 2>&1 || true)

# --- Parse pool-level fields ---

# Total device count from "Total devices N" line
total_devices=$(echo "$fi_show" | awk '/Total devices/ {for(i=1;i<=NF;i++) if($i=="devices") {print $(i+1); exit}}')

# Present devices: lines with /dev/mapper/ paths
present_count=$(echo "$fi_show" | awk '/\/dev\/mapper\// {c++} END {print c+0}')
missing_count=$((total_devices - present_count))

# Profile from Data line in btrfs fi df
profile=$(echo "$fi_df" | awk '/^Data,/ {sub("^Data, *", ""); sub(":.*", ""); print}')

# Capacity (bytes from --raw output)
format_bytes() {
  awk -v bytes="$1" 'BEGIN {
    if (bytes >= 1099511627776) printf "%.2f TiB", bytes/1099511627776
    else if (bytes >= 1073741824) printf "%.2f GiB", bytes/1073741824
    else if (bytes >= 1048576) printf "%.2f MiB", bytes/1048576
    else if (bytes >= 1024) printf "%.2f KiB", bytes/1024
    else printf "%d B", bytes
  }'
}

total_bytes=$(echo "$fi_usage" | awk '/Device size:/ {print $NF; exit}')
used_bytes=$(echo "$fi_usage" | awk '/^[[:space:]]+Used:/ {print $NF; exit}')
free_bytes=$(echo "$fi_usage" | awk '/Free \(estimated\):/ {print $NF; exit}')

# Scrub status
if echo "$scrub_output" | grep -qi "no stats available"; then
  last_scrub="never"
else
  last_scrub=$(echo "$scrub_output" | awk '/[Ss]crub started/ {sub(".*scrub started at ", ""); print; exit}')
  [[ -z "$last_scrub" ]] && last_scrub="unknown"
fi

# --- Determine status ---

if (( missing_count > 0 )); then
  if (( missing_count == 1 )); then
    status="DEGRADED ($missing_count missing device)"
  else
    status="DEGRADED ($missing_count missing devices)"
  fi
  drives_line="$present_count present, $missing_count missing"
else
  status="healthy"
  drives_line="$total_devices"
fi

# --- Print summary ---

echo "Pool:     $MOUNT_POINT"
echo "Status:   $status"
echo "Drives:   $drives_line"
echo "Profile:  $profile"
echo ""
echo "Capacity:"
echo "  Total:  $(format_bytes "$total_bytes")"
echo "  Used:   $(format_bytes "$used_bytes")"
echo "  Free:   $(format_bytes "$free_bytes")"
echo ""
echo "Last scrub: $last_scrub"

# --- Verbose: per-disk detail ---

if ! $VERBOSE; then
  exit 0
fi

dev_stats=$(btrfs device stats "$MOUNT_POINT" 2>&1 || true)

echo ""
echo "Disks:"

# Parse present devices into parallel arrays
mapfile -t present_mappers < <(echo "$fi_show" | awk '/\/dev\/mapper\// {print $NF}' | sed 's|.*/||')
mapfile -t present_devids < <(echo "$fi_show" | awk '/\/dev\/mapper\// {for(i=1;i<=NF;i++) if($i=="devid") {print $(i+1); break}}')

# Print present disks
for i in "${!present_mappers[@]}"; do
  mapper="${present_mappers[$i]}"
  devid="${present_devids[$i]}"

  # Find by-id path from config (mapper name = basename of by-id path)
  by_id=""
  while IFS= read -r d; do
    if [[ "$(basename "$d")" == "$mapper" ]]; then
      by_id="$d"
      break
    fi
  done < <(jq -r '.disks[]' "$CONFIG_FILE")

  echo "  $mapper      devid $devid   present"

  if [[ -n "$by_id" ]]; then
    echo "    Device:  $by_id"
    real_dev=$(readlink -f "$by_id" 2>/dev/null || true)
    if [[ -n "$real_dev" && -b "$real_dev" ]]; then
      model=$(lsblk -ndo MODEL "$real_dev" 2>/dev/null | xargs || true)
      serial=$(lsblk -ndo SERIAL "$real_dev" 2>/dev/null | xargs || true)
      echo "    Model:   ${model:-(unknown)}"
      echo "    Serial:  ${serial:-(unknown)}"
    fi
    luks_uuid=$(cryptsetup luksUUID "$by_id" 2>/dev/null || true)
    echo "    LUKS:    ${luks_uuid:-(unknown)}"
  else
    echo "    Device:  /dev/mapper/$mapper  (not in config)"
  fi

  # Error counters from btrfs device stats
  read_errs=$(echo "$dev_stats" | awk -v m="$mapper" 'index($0, "[/dev/mapper/"m"].read_io_errs") {print $NF}')
  write_errs=$(echo "$dev_stats" | awk -v m="$mapper" 'index($0, "[/dev/mapper/"m"].write_io_errs") {print $NF}')
  corrupt_errs=$(echo "$dev_stats" | awk -v m="$mapper" 'index($0, "[/dev/mapper/"m"].corruption_errs") {print $NF}')
  echo "    Errors:  read ${read_errs:-0} / write ${write_errs:-0} / corruption ${corrupt_errs:-0}"
  echo ""
done

# Missing disks: configured but not present, only when pool has missing devices
if (( missing_count > 0 )); then
  while IFS= read -r d; do
    disk_basename=$(basename "$d")
    found=false
    for mapper in "${present_mappers[@]}"; do
      if [[ "$mapper" == "$disk_basename" ]]; then
        found=true
        break
      fi
    done
    if ! $found; then
      echo "  $disk_basename      MISSING"
      echo "    Device:  $d  (not found)"
      echo "    Errors:  unknown (device absent)"
      echo ""
    fi
  done < <(jq -r '.disks[]' "$CONFIG_FILE")
fi
