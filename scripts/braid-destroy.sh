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
pool_json="/var/lib/braid/pool.json"

# Pool membership lives in pool.json (CLI-owned runtime state);
# /etc/braid/config.json only holds mount_point/fan_control/ups.
if [ ! -f "$pool_json" ]; then
    echo "Error: $pool_json not found -- no pool to destroy." >&2
    echo "If you want to clear residual state, run: sudo rm -rf /var/lib/braid/" >&2
    exit 1
fi

# Validate membership and emit name<TAB>by_id tuples. Any reject arm aborts
# before braid lock, wipefs, or rm -rf.
# shellcheck disable=SC2016  # $path and $e are jq expressions, not shell
read_filter='
  if (.disks // {} | length) == 0 then
    "pool has no disks in \($path)\n" | halt_error(1)
  else
    .disks | to_entries[] as $e
    | if ($e.value.by_id // "") == "" then
        "disk \"\($e.key)\" has no by_id in \($path)\n" | halt_error(1)
      else
        [$e.key, $e.value.by_id] | @tsv
      end
  end'

tsv="$(jq -r --arg path "$pool_json" "$read_filter" "$pool_json")" || exit 1

declare -a keys=()
declare -A by_id=()
while IFS=$'\t' read -r name path; do
    keys+=("$name")
    by_id[$name]="$path"
done <<< "$tsv"

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
