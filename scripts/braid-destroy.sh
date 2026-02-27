#!/usr/bin/env bash
set -euo pipefail

# Destroy an entire braid pool: wipe LUKS signatures + state files.
# Dev use only — not shipped as part of braid.

# Preflight: check required tools
missing=()
for cmd in jq lsblk wipefs braid; do
    command -v "$cmd" &>/dev/null || missing+=("$cmd")
done
if [ ${#missing[@]} -gt 0 ]; then
    echo "Error: missing required tools: ${missing[*]}" >&2
    exit 1
fi

config="${1:-/etc/braid/config.json}"

# Read disk keys and by_id paths from config
mapfile -t keys < <(jq -r '.disks | keys[]' "$config")
declare -A by_id
for key in "${keys[@]}"; do
    by_id[$key]=$(jq -r ".disks[\"$key\"].by_id" "$config")
done

# Print summary table
echo "This will destroy the entire braid pool:"
for key in "${keys[@]}"; do
    size=$(lsblk --nodeps --noheadings --output SIZE "${by_id[$key]}" 2>/dev/null || echo "???")
    # Trim whitespace from lsblk output
    size=$(echo "$size" | xargs)
    printf "  %-12s %-50s %s\n" "$key" "${by_id[$key]}" "$size"
done
echo "State directory to delete:"
echo "  /var/lib/braid/"

# Confirm
printf "\nType YES to confirm: "
read -r answer
if [ "$answer" != "YES" ]; then
    echo "Aborted."
    exit 1
fi

# Lock the pool (no-op if already locked)
sudo braid lock --config "$config"

# Wipe LUKS + btrfs signatures from each disk
for key in "${keys[@]}"; do
    echo "Wiping ${by_id[$key]} ..."
    sudo wipefs -a "${by_id[$key]}"
done

# Remove all state
sudo rm -rf /var/lib/braid/

echo "Done. Pool destroyed."
