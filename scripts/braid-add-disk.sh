#!/usr/bin/env bash
# braid-add-disk — DEPRECATED wrapper for braid init-disk + braid apply.
#
# This script is deprecated. Use the new two-step workflow instead:
#   1. braid init-disk /dev/disk/by-id/<device>
#   2. braid apply
#
# This wrapper preserves backward compatibility by calling init-disk + apply.

set -euo pipefail

echo "WARNING: braid-add-disk is deprecated." >&2
echo "Use instead:" >&2
echo "  1. braid init-disk <by-id-path>" >&2
echo "  2. braid apply" >&2
echo "" >&2

# --- Parse config flag ---

CONFIG_FILE="/etc/braid/config.json"
config_flag=""
if [[ "${1:-}" == "--config" ]]; then
  CONFIG_FILE="$2"
  config_flag="--config $CONFIG_FILE"
  shift 2
fi

# --- No-args mode: list disks (preserved for discoverability) ---

if [[ $# -eq 0 ]]; then
  echo "Usage: braid-add-disk [--config <path>] <disk>"
  echo ""
  echo "Configured disks:"
  jq -r '.disks[]' "$CONFIG_FILE" 2>/dev/null | while IFS= read -r d; do
    if [[ -b "$d" ]]; then
      echo "  $d  (present)"
    else
      echo "  $d  (NOT FOUND)"
    fi
  done
  echo ""
  echo "Preferred workflow:"
  echo "  braid init-disk <by-id-path>"
  echo "  braid apply"
  exit 0
fi

if [[ $# -ne 1 ]]; then
  echo "Usage: braid-add-disk [--config <path>] <disk>" >&2
  exit 1
fi

disk="$1"

# --- Require confirmation (backward-compatible "erase this disk" prompt) ---

echo ""
echo "This will format $disk with LUKS and add it to the pool."
echo "Type 'erase this disk' to confirm:"

read -r confirmation
if [[ "$confirmation" != "erase this disk" ]]; then
  echo "Aborted."
  exit 1
fi

# --- Step 1: init-disk ---

# Use --force if disk already has LUKS header (matches old behavior of re-formatting)
force_flag=""
if cryptsetup isLuks "$disk" 2>/dev/null; then
  export BRAID_CONFIRM="reformat this disk"
  force_flag="--force"
fi

# shellcheck disable=SC2086
braid init-disk $config_flag $force_flag "$disk"

# --- Step 2: apply ---

# shellcheck disable=SC2086
braid apply $config_flag
