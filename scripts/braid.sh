#!/usr/bin/env bash
# braid — unified CLI for braid disk pool management.
# Usage: braid <command> [options]
#   braid plan [--json] [--config <path>]
#   braid apply [--resume] [--config <path>]
#   braid status [--verbose] [--json] [--config <path>]

set -euo pipefail

# --- Global state ---

CONFIG_FILE="/etc/braid/config.json"

# ============================================================================
# Shared helpers
# ============================================================================

usage() {
  echo "Usage: braid <command> [options]"
  echo ""
  echo "Commands:"
  echo "  plan     Preview what actions braid will take"
  echo "  apply    Execute the plan"
  echo "  status   Show pool health"
  echo ""
  echo "Global options:"
  echo "  --config <path>   Config file (default: /etc/braid/config.json)"
  exit 1
}

die() { echo "Error: $*" >&2; exit 1; }

config_read() {
  [[ -f "$CONFIG_FILE" ]] || die "$CONFIG_FILE not found. Is the braid module enabled?"
}

config_disks() {
  jq -r '.disks[]' "$CONFIG_FILE"
}

config_mount_point() {
  jq -r '.mountPoint' "$CONFIG_FILE"
}

config_hash() {
  sha256sum "$CONFIG_FILE" | awk '{print "sha256:" $1}'
}

# Resolve /dev/disk/by-id/X to its real device path, or empty string if absent.
resolve_by_id() {
  local by_id="$1"
  if [[ -b "$by_id" ]]; then
    readlink -f "$by_id"
  else
    echo ""
  fi
}

format_bytes() {
  awk -v bytes="$1" 'BEGIN {
    if (bytes >= 1099511627776) printf "%.2f TiB", bytes/1099511627776
    else if (bytes >= 1073741824) printf "%.2f GiB", bytes/1073741824
    else if (bytes >= 1048576) printf "%.2f MiB", bytes/1048576
    else if (bytes >= 1024) printf "%.2f KiB", bytes/1024
    else printf "%d B", bytes
  }'
}

# ============================================================================
# Live state discovery
# ============================================================================

# Discover the current state of the pool and LUKS devices.
# Outputs a JSON blob to stdout.
discover_live_state() {
  local mount_point
  mount_point=$(config_mount_point)

  local mounted=false
  local pool_json="[]"
  local missing_count=0
  local total_devices=0
  local profile=""

  if mountpoint -q "$mount_point" 2>/dev/null; then
    local mount_type
    mount_type=$(findmnt -n -o FSTYPE "$mount_point" 2>/dev/null || true)
    if [[ "$mount_type" == "btrfs" ]]; then
      mounted=true
    fi
  fi

  if $mounted; then
    local fi_show
    fi_show=$(btrfs filesystem show "$mount_point")

    # Total devices
    total_devices=$(echo "$fi_show" | awk '/Total devices/ {for(i=1;i<=NF;i++) if($i=="devices") {print $(i+1); exit}}')

    # Present mapper devices with devids
    local mappers devids
    mapfile -t mappers < <(echo "$fi_show" | awk '/\/dev\/mapper\// {print $NF}' | sed 's|.*/||')
    mapfile -t devids < <(echo "$fi_show" | awk '/\/dev\/mapper\// {for(i=1;i<=NF;i++) if($i=="devid") {print $(i+1); break}}')

    # Build pool devices JSON array
    pool_json="["
    for i in "${!mappers[@]}"; do
      local mapper="${mappers[$i]}"
      local devid="${devids[$i]}"
      local by_id="/dev/disk/by-id/$mapper"
      [[ $i -gt 0 ]] && pool_json+=","
      pool_json+=$(jq -n \
        --arg mapper "$mapper" \
        --arg by_id "$by_id" \
        --arg devid "$devid" \
        '{mapper: $mapper, by_id: $by_id, devid: $devid}')
    done
    pool_json+="]"

    # Missing count
    local present_count=${#mappers[@]}
    missing_count=$((total_devices - present_count))

    # Profile
    local fi_df
    fi_df=$(btrfs filesystem df "$mount_point" 2>/dev/null || true)
    profile=$(echo "$fi_df" | awk '/^Data,/ {sub("^Data, *", ""); sub(":.*", ""); print}')
  fi

  # Open LUKS mappers for configured disks
  local open_mappers="[]"
  local mapper_arr=()
  while IFS= read -r disk; do
    local mapper
    mapper=$(basename "$disk")
    if [[ -e "/dev/mapper/$mapper" ]] && cryptsetup status "$mapper" >/dev/null 2>&1; then
      mapper_arr+=("$mapper")
    fi
  done < <(config_disks)

  open_mappers=$(printf '%s\n' "${mapper_arr[@]}" | jq -R . | jq -s .)

  jq -n \
    --arg mount_point "$mount_point" \
    --argjson mounted "$mounted" \
    --argjson pool_devices "$pool_json" \
    --argjson missing_count "$missing_count" \
    --argjson total_devices "$total_devices" \
    --arg profile "$profile" \
    --argjson open_luks_mappers "$open_mappers" \
    '{
      mount_point: $mount_point,
      mounted: $mounted,
      pool_devices: $pool_devices,
      missing_count: $missing_count,
      total_devices: $total_devices,
      profile: $profile,
      open_luks_mappers: $open_luks_mappers
    }'
}

# ============================================================================
# Planner: compute_plan
# ============================================================================

generate_plan_id() {
  local ts hash
  ts=$(date -u +"%Y-%m-%dT%H:%M:%SZ")
  hash=$(echo -n "$ts-$$-$RANDOM" | sha256sum | cut -c1-6)
  echo "${ts}-${hash}"
}

# Compute the diff between desired state (config) and live state.
# Args: $1 = live_state JSON (from discover_live_state)
# Outputs: plan JSON to stdout.
compute_plan() {
  local live_state="$1"
  local mount_point
  mount_point=$(config_mount_point)

  local mounted
  mounted=$(echo "$live_state" | jq -r '.mounted')

  if [[ "$mounted" != "true" ]]; then
    die "Pool is not mounted at $mount_point. Cannot compute plan."
  fi

  local missing_count
  missing_count=$(echo "$live_state" | jq -r '.missing_count')

  # Get pool mapper names
  local pool_mappers
  mapfile -t pool_mappers < <(echo "$live_state" | jq -r '.pool_devices[].mapper')

  # Get configured disk basenames
  local config_basenames=()
  local config_disk_map=() # parallel array: full by-id path
  while IFS= read -r disk; do
    config_basenames+=("$(basename "$disk")")
    config_disk_map+=("$disk")
  done < <(config_disks)

  local actions=()
  local confirmations=()
  local warnings=()
  # Use a temp file for the counter since $(next_id) runs in a subshell
  local counter_file
  counter_file=$(mktemp)
  echo 0 > "$counter_file"

  next_id() {
    local c
    c=$(cat "$counter_file")
    c=$((c + 1))
    echo "$c" > "$counter_file"
    echo "a${c}"
  }

  # --- Disks to ADD: in config but not in pool ---
  local disks_to_add=()
  for i in "${!config_basenames[@]}"; do
    local basename="${config_basenames[$i]}"
    local by_id="${config_disk_map[$i]}"
    local in_pool=false

    for pm in "${pool_mappers[@]}"; do
      if [[ "$pm" == "$basename" ]]; then
        in_pool=true
        break
      fi
    done

    if ! $in_pool; then
      disks_to_add+=("$by_id")

      # Check if LUKS is already formatted+open (crash recovery)
      local is_open
      is_open=$(echo "$live_state" | jq -r --arg m "$basename" '.open_luks_mappers | index($m) != null')

      if [[ "$is_open" != "true" ]]; then
        actions+=("$(jq -n \
          --arg id "$(next_id)" \
          --arg target "$by_id" \
          '{id: $id, type: "ADD_DISK_LUKS_FORMAT_OPEN", target: $target, preconditions: ["device_present", "device_not_luks"], status: "pending"}')")
      fi

      actions+=("$(jq -n \
        --arg id "$(next_id)" \
        --arg target "/dev/mapper/$basename" \
        '{id: $id, type: "ADD_DISK_BTRFS_ADD", target: $target, preconditions: ["mapper_open", "not_in_pool"], status: "pending"}')")
    fi
  done

  # --- Disks to REMOVE: in pool but not in config ---
  local disks_to_remove=()
  for pm in "${pool_mappers[@]}"; do
    local in_config=false
    for cb in "${config_basenames[@]}"; do
      if [[ "$cb" == "$pm" ]]; then
        in_config=true
        break
      fi
    done

    if ! $in_config; then
      disks_to_remove+=("$pm")
      actions+=("$(jq -n \
        --arg id "$(next_id)" \
        --arg target "/dev/mapper/$pm" \
        '{id: $id, type: "REMOVE_DISK_GRACEFUL", target: $target, preconditions: ["target_mapper_open", "target_in_pool"], status: "pending"}')")
      actions+=("$(jq -n \
        --arg id "$(next_id)" \
        --arg target "$pm" \
        '{id: $id, type: "CLOSE_LUKS_MAPPER", target: $target, preconditions: ["mapper_open"], status: "pending"}')")
    fi
  done

  # --- Missing device removal ---
  if (( missing_count > 0 )); then
    # If there are multiple missing devices, refuse (ambiguous)
    if (( missing_count > 1 )); then
      rm -f "$counter_file"
      die "Multiple missing devices detected ($missing_count). Cannot determine which device to remove. Resolve manually: btrfs device remove missing $mount_point"
    fi

    # Single missing device — emit a REMOVE_DISK_MISSING action.
    actions+=("$(jq -n \
      --arg id "$(next_id)" \
      --arg target "missing" \
      '{id: $id, type: "REMOVE_DISK_MISSING", target: $target, preconditions: ["pool_has_missing"], status: "pending"}')")
  fi

  # --- BALANCE_TO_RAID1 if adding disks and pool will have 2+ devices ---
  if (( ${#disks_to_add[@]} > 0 )); then
    local current_pool_size=${#pool_mappers[@]}
    local after_add=$((current_pool_size + ${#disks_to_add[@]} - ${#disks_to_remove[@]} - missing_count))
    if (( after_add >= 2 )); then
      actions+=("$(jq -n \
        --arg id "$(next_id)" \
        --arg target "$mount_point" \
        '{id: $id, type: "BALANCE_TO_RAID1", target: $target, preconditions: ["pool_has_2_plus_devices"], status: "pending"}')")
    fi
  fi

  # --- Redundancy warning ---
  if (( ${#disks_to_remove[@]} > 0 || missing_count > 0 )); then
    local total_with_missing
    total_with_missing=$(echo "$live_state" | jq -r '.total_devices')
    local after_remove=$((total_with_missing - ${#disks_to_remove[@]} - missing_count + ${#disks_to_add[@]}))
    if (( after_remove < 2 )); then
      local remove_action_id=""
      for a_json in "${actions[@]}"; do
        local atype
        atype=$(echo "$a_json" | jq -r '.type')
        if [[ "$atype" == "REMOVE_DISK_GRACEFUL" || "$atype" == "REMOVE_DISK_MISSING" ]]; then
          remove_action_id=$(echo "$a_json" | jq -r '.id')
          break
        fi
      done
      confirmations+=("$(jq -n \
        --arg action_id "$remove_action_id" \
        '{action_id: $action_id, phrase: "remove this disk without redundancy"}')")
    fi
  fi

  # --- Verify actions (always appended) ---
  actions+=("$(jq -n \
    --arg id "$(next_id)" \
    --arg target "$mount_point" \
    '{id: $id, type: "VERIFY_POOL_HEALTH", target: $target, preconditions: [], status: "pending"}')")
  actions+=("$(jq -n \
    --arg id "$(next_id)" \
    --arg target "$mount_point" \
    '{id: $id, type: "VERIFY_EXPECTED_DISK_SET", target: $target, preconditions: [], status: "pending"}')")

  rm -f "$counter_file"

  # --- Assemble plan JSON ---
  local plan_id
  plan_id=$(generate_plan_id)

  local actions_json="["
  for i in "${!actions[@]}"; do
    [[ $i -gt 0 ]] && actions_json+=","
    actions_json+="${actions[$i]}"
  done
  actions_json+="]"

  local confirmations_json="["
  for i in "${!confirmations[@]}"; do
    [[ $i -gt 0 ]] && confirmations_json+=","
    confirmations_json+="${confirmations[$i]}"
  done
  confirmations_json+="]"

  local warnings_json="["
  for i in "${!warnings[@]}"; do
    [[ $i -gt 0 ]] && warnings_json+=","
    warnings_json+="\"${warnings[$i]}\""
  done
  warnings_json+="]"

  jq -n \
    --argjson schema_version 1 \
    --arg plan_id "$plan_id" \
    --arg mount_point "$mount_point" \
    --argjson warnings "$warnings_json" \
    --argjson confirmations "$confirmations_json" \
    --argjson actions "$actions_json" \
    '{
      schema_version: $schema_version,
      plan_id: $plan_id,
      mount_point: $mount_point,
      warnings: $warnings,
      confirmations: $confirmations,
      actions: $actions
    }'
}

# ============================================================================
# Plan output formatting
# ============================================================================

format_plan_human() {
  local plan_json="$1"

  local plan_id mount_point
  plan_id=$(echo "$plan_json" | jq -r '.plan_id')
  mount_point=$(echo "$plan_json" | jq -r '.mount_point')

  local mutation_count
  mutation_count=$(echo "$plan_json" | jq '[.actions[] | select(.type | startswith("VERIFY_") | not)] | length')

  echo "Plan ID: $plan_id"
  echo "Mount: $mount_point"
  echo "Actions: $mutation_count"
  echo ""

  if (( mutation_count == 0 )); then
    echo "No actions required — pool matches config."
    echo ""
    return
  fi

  local i=0
  while IFS= read -r line; do
    i=$((i + 1))
    local atype target
    atype=$(echo "$line" | jq -r '.type')
    target=$(echo "$line" | jq -r '.target')
    printf "[%d] %-30s target=%s\n" "$i" "$atype" "$target"
  done < <(echo "$plan_json" | jq -c '.actions[]')

  echo ""

  # Warnings
  local warning_count
  warning_count=$(echo "$plan_json" | jq '.warnings | length')
  if (( warning_count > 0 )); then
    echo "Warnings:"
    echo "$plan_json" | jq -r '.warnings[]' | while IFS= read -r w; do
      echo "  - $w"
    done
    echo ""
  else
    echo "Warnings: none"
  fi

  # Confirmations
  local confirm_count
  confirm_count=$(echo "$plan_json" | jq '.confirmations | length')
  if (( confirm_count > 0 )); then
    echo "Confirmations required: $confirm_count"
  fi

  echo "Next step: run 'sudo braid apply'"
}

# ============================================================================
# cmd_plan
# ============================================================================

cmd_plan() {
  local json_output=false

  while [[ $# -gt 0 ]]; do
    case "$1" in
      --json) json_output=true; shift ;;
      --config) CONFIG_FILE="$2"; shift 2 ;;
      *) die "Unknown option for plan: $1" ;;
    esac
  done

  config_read

  local live_state
  live_state=$(discover_live_state)

  local plan
  plan=$(compute_plan "$live_state")

  if $json_output; then
    echo "$plan" | jq .
  else
    format_plan_human "$plan"
  fi
}

# ============================================================================
# Checkpoint helpers
# ============================================================================

CHECKPOINT_DIR="/var/lib/braid"
CHECKPOINT_FILE="$CHECKPOINT_DIR/apply-state.json"
HISTORY_DIR="$CHECKPOINT_DIR/history"
HISTORY_KEEP=20

checkpoint_init() {
  local plan_json="$1"
  mkdir -p "$CHECKPOINT_DIR" "$HISTORY_DIR"
  local now
  now=$(date -u +"%Y-%m-%dT%H:%M:%SZ")
  echo "$plan_json" | jq \
    --arg created_at "$now" \
    --arg updated_at "$now" \
    --arg config_hash "$(config_hash)" \
    '. + {created_at: $created_at, updated_at: $updated_at, config_hash: $config_hash, last_completed_action_id: ""}' \
    > "${CHECKPOINT_FILE}.tmp" && mv "${CHECKPOINT_FILE}.tmp" "$CHECKPOINT_FILE"
}

checkpoint_update_action() {
  local action_id="$1" status="$2"
  local now
  now=$(date -u +"%Y-%m-%dT%H:%M:%SZ")
  jq --arg id "$action_id" --arg s "$status" --arg now "$now" '
    .actions = [.actions[] | if .id == $id then
      .status = $s
      | if $s == "in_progress" then .started_at = $now else . end
      | if $s == "completed" then .completed_at = $now else . end
    else . end]
    | if $s == "completed" then .last_completed_action_id = $id else . end
    | .updated_at = $now
  ' "$CHECKPOINT_FILE" > "${CHECKPOINT_FILE}.tmp" && mv "${CHECKPOINT_FILE}.tmp" "$CHECKPOINT_FILE"
}

checkpoint_finalize() {
  local plan_id
  plan_id=$(jq -r '.plan_id' "$CHECKPOINT_FILE")
  cp "$CHECKPOINT_FILE" "$HISTORY_DIR/${plan_id}.json"
  rm -f "$CHECKPOINT_FILE"
  # Prune old history files (keep newest HISTORY_KEEP)
  local history_files=()
  mapfile -t history_files < <(find "$HISTORY_DIR" -maxdepth 1 -name '*.json' -type f | sort -r)
  local count=${#history_files[@]}
  if (( count > HISTORY_KEEP )); then
    local i
    for (( i=HISTORY_KEEP; i<count; i++ )); do
      rm -f "${history_files[$i]}"
    done
  fi
}

# ============================================================================
# Action handlers
# ============================================================================

action_luks_format_open() {
  local target="$1"  # by-id path
  local mapper_name
  mapper_name=$(basename "$target")

  local passphrase="${BRAID_PASSPHRASE:-}"
  if [[ -z "$passphrase" ]]; then
    die "BRAID_PASSPHRASE is required for ADD operations."
  fi

  # Parse BRAID_LUKS_OPTS
  local luks_extra_opts=()
  if [[ -n "${BRAID_LUKS_OPTS:-}" ]]; then
    read -ra luks_extra_opts <<< "$BRAID_LUKS_OPTS"
  fi

  echo "Formatting $target with LUKS..."
  echo -n "$passphrase" | cryptsetup luksFormat --batch-mode --key-file=- "${luks_extra_opts[@]}" "$target"

  echo "Opening LUKS device as $mapper_name..."
  echo -n "$passphrase" | cryptsetup luksOpen --key-file=- "$target" "$mapper_name"
}

action_btrfs_add() {
  local target="$1"  # mapper path like /dev/mapper/virtio-disk3
  local mount_point
  mount_point=$(config_mount_point)

  # Check if this is the first disk (pool doesn't exist yet) or adding to existing
  local pool_devs
  pool_devs=$(btrfs filesystem show "$mount_point" 2>/dev/null | grep -c "devid" || echo "0")

  if (( pool_devs == 0 )); then
    echo "Creating btrfs filesystem on $target..."
    mkfs.btrfs -f "$target"
    mkdir -p "$mount_point"
    mount "$target" "$mount_point"
  else
    echo "Adding $target to btrfs pool..."
    btrfs device add "$target" "$mount_point"
  fi
}

action_balance_raid1() {
  local target="$1"  # mount point
  echo "Starting RAID1 balance (this may take a while on large pools)..."
  btrfs balance start -dconvert=raid1 -mconvert=raid1 "$target"
  # Evict any dead devices left in the pool
  if btrfs filesystem show "$target" 2>/dev/null | grep -qi "missing"; then
    echo "Removing missing (dead) device from pool..."
    btrfs device remove missing "$target"
  fi
}

action_remove_graceful() {
  local target="$1"  # mapper path like /dev/mapper/virtio-disk3
  local mount_point
  mount_point=$(config_mount_point)

  # Check if we need to convert from RAID1 to single first
  local device_count
  device_count=$(btrfs filesystem show "$mount_point" 2>/dev/null | grep -c "devid" || echo "0")
  local remaining=$((device_count - 1))

  if (( remaining < 2 )); then
    echo "Converting pool from RAID1 to single profile..."
    btrfs balance start -dconvert=single -mconvert=single -f "$mount_point"
  fi

  echo "Removing $target from btrfs pool (migrating data off)..."
  btrfs device remove "$target" "$mount_point"
}

action_remove_missing() {
  local mount_point
  mount_point=$(config_mount_point)
  echo "Removing missing device from btrfs pool..."
  btrfs device remove missing "$mount_point"
}

action_close_luks() {
  local target="$1"  # mapper name
  echo "Closing LUKS device $target..."
  if ! cryptsetup close "$target"; then
    echo "Warning: Could not close LUKS device $target. It will close on reboot."
  fi
}

action_verify_health() {
  local mount_point
  mount_point=$(config_mount_point)
  if ! mountpoint -q "$mount_point" 2>/dev/null; then
    die "Pool is not mounted at $mount_point after apply."
  fi
  local fi_show
  fi_show=$(btrfs filesystem show "$mount_point")
  if echo "$fi_show" | grep -qi "missing"; then
    echo "Warning: Pool still has missing devices after apply."
  else
    echo "Pool health verified: no missing devices."
  fi
}

action_verify_diskset() {
  local mount_point
  mount_point=$(config_mount_point)
  local fi_show
  fi_show=$(btrfs filesystem show "$mount_point")

  # Check that every configured disk is in the pool
  while IFS= read -r disk; do
    local mapper
    mapper=$(basename "$disk")
    if ! echo "$fi_show" | grep -q "/dev/mapper/$mapper"; then
      echo "Warning: configured disk $disk is not in pool."
    fi
  done < <(config_disks)
  echo "Disk set verification complete."
}

# ============================================================================
# cmd_apply
# ============================================================================

cmd_apply() {
  local resume=false

  while [[ $# -gt 0 ]]; do
    case "$1" in
      --resume) resume=true; shift ;;
      --config) CONFIG_FILE="$2"; shift 2 ;;
      *) die "Unknown option for apply: $1" ;;
    esac
  done

  config_read

  local plan_json

  if $resume; then
    # Resume from checkpoint
    if [[ ! -f "$CHECKPOINT_FILE" ]]; then
      die "No checkpoint found at $CHECKPOINT_FILE. Run 'braid apply' (without --resume) first."
    fi

    plan_json=$(cat "$CHECKPOINT_FILE")

    # Verify config hash
    local saved_hash current_hash
    saved_hash=$(echo "$plan_json" | jq -r '.config_hash')
    current_hash=$(config_hash)
    if [[ "$saved_hash" != "$current_hash" ]]; then
      die "Config has changed since the checkpoint was created. Run 'braid plan' to review, then 'braid apply' (without --resume)."
    fi

    echo "Resuming apply from checkpoint..."
  else
    # Fresh apply — run planner
    if [[ -f "$CHECKPOINT_FILE" ]]; then
      die "An apply is already in progress (checkpoint exists at $CHECKPOINT_FILE). Use 'braid apply --resume' to continue, or delete the checkpoint to start fresh."
    fi

    local live_state
    live_state=$(discover_live_state)
    plan_json=$(compute_plan "$live_state")

    # Check for no-op
    local mutation_count
    mutation_count=$(echo "$plan_json" | jq '[.actions[] | select(.type | startswith("VERIFY_") | not)] | length')
    if (( mutation_count == 0 )); then
      echo "Nothing to do — pool matches config."
      return
    fi

    # Check confirmations
    local confirm_count
    confirm_count=$(echo "$plan_json" | jq '.confirmations | length')
    if (( confirm_count > 0 )); then
      local confirm_phrase
      confirm_phrase=$(echo "$plan_json" | jq -r '.confirmations[0].phrase')
      local provided="${BRAID_CONFIRM:-}"
      if [[ -z "$provided" ]]; then
        die "This operation requires confirmation. Set BRAID_CONFIRM='$confirm_phrase' or use interactive mode."
      fi
      if [[ "$provided" != "$confirm_phrase" ]]; then
        die "Confirmation phrase doesn't match. Expected: '$confirm_phrase'"
      fi
    fi

    # Write checkpoint
    checkpoint_init "$plan_json"
    plan_json=$(cat "$CHECKPOINT_FILE")
  fi

  # Execute actions
  local action_ids
  mapfile -t action_ids < <(echo "$plan_json" | jq -r '.actions[].id')

  for action_id in "${action_ids[@]}"; do
    local action_json action_status action_type action_target
    action_json=$(jq -c --arg id "$action_id" '[.actions[] | select(.id == $id)][0]' "$CHECKPOINT_FILE")
    action_status=$(echo "$action_json" | jq -r '.status')
    action_type=$(echo "$action_json" | jq -r '.type')
    action_target=$(echo "$action_json" | jq -r '.target')

    # Skip completed actions
    if [[ "$action_status" == "completed" ]]; then
      echo "[$action_id] $action_type — already completed, skipping."
      continue
    fi

    echo ""
    echo "[$action_id] $action_type target=$action_target"
    checkpoint_update_action "$action_id" "in_progress"

    case "$action_type" in
      ADD_DISK_LUKS_FORMAT_OPEN) action_luks_format_open "$action_target" ;;
      ADD_DISK_BTRFS_ADD)        action_btrfs_add "$action_target" ;;
      BALANCE_TO_RAID1)          action_balance_raid1 "$action_target" ;;
      REMOVE_DISK_GRACEFUL)      action_remove_graceful "$action_target" ;;
      REMOVE_DISK_MISSING)       action_remove_missing ;;
      CLOSE_LUKS_MAPPER)         action_close_luks "$action_target" ;;
      VERIFY_POOL_HEALTH)        action_verify_health ;;
      VERIFY_EXPECTED_DISK_SET)  action_verify_diskset ;;
      *) die "Unknown action type: $action_type" ;;
    esac

    checkpoint_update_action "$action_id" "completed"

    # Test hook: simulate failure after completing specific action
    if [[ -n "${BRAID_TEST_FAIL_AFTER_ACTION:-}" && "$action_id" == "$BRAID_TEST_FAIL_AFTER_ACTION" ]]; then
      die "Test hook: simulated failure after action $action_id"
    fi
  done

  echo ""
  echo "Apply complete."
  checkpoint_finalize
}

# ============================================================================
# cmd_status
# ============================================================================

cmd_status() {
  local verbose=false
  local json_output=false

  while [[ $# -gt 0 ]]; do
    case "$1" in
      --verbose) verbose=true; shift ;;
      --json) json_output=true; shift ;;
      --config) CONFIG_FILE="$2"; shift 2 ;;
      *) die "Unknown option for status: $1" ;;
    esac
  done

  config_read

  local mount_point
  mount_point=$(config_mount_point)

  # Validate pool is mounted
  if ! mountpoint -q "$mount_point" 2>/dev/null; then
    die "$mount_point is not mounted."
  fi

  local mount_type
  mount_type=$(findmnt -n -o FSTYPE "$mount_point" 2>/dev/null || true)
  if [[ "$mount_type" != "btrfs" ]]; then
    die "$mount_point is not a btrfs filesystem."
  fi

  # Gather btrfs data
  local fi_show fi_df fi_usage scrub_output
  fi_show=$(btrfs filesystem show "$mount_point")
  fi_df=$(btrfs filesystem df "$mount_point")
  fi_usage=$(btrfs filesystem usage --raw "$mount_point")
  scrub_output=$(btrfs scrub status "$mount_point" 2>&1 || true)

  # Parse pool-level fields
  local total_devices present_count missing_count profile
  total_devices=$(echo "$fi_show" | awk '/Total devices/ {for(i=1;i<=NF;i++) if($i=="devices") {print $(i+1); exit}}')

  present_count=$(echo "$fi_show" | awk '/\/dev\/mapper\// {c++} END {print c+0}')
  missing_count=$((total_devices - present_count))

  profile=$(echo "$fi_df" | awk '/^Data,/ {sub("^Data, *", ""); sub(":.*", ""); print}')

  # Capacity (raw bytes — extract first purely numeric field after the label)
  local total_bytes used_bytes free_bytes
  total_bytes=$(echo "$fi_usage" | awk '/Device size:/ {for(i=1;i<=NF;i++) if($i ~ /^[0-9]+$/) {print $i; exit}}')
  used_bytes=$(echo "$fi_usage" | awk '/^[[:space:]]+Used:/ {for(i=1;i<=NF;i++) if($i ~ /^[0-9]+$/) {print $i; exit}}')
  free_bytes=$(echo "$fi_usage" | awk '/Free \(estimated\):/ {for(i=1;i<=NF;i++) if($i ~ /^[0-9]+$/) {print $i; exit}}')

  # Scrub status
  local last_scrub
  if echo "$scrub_output" | grep -qi "no stats available"; then
    last_scrub="never"
  else
    last_scrub=$(echo "$scrub_output" | awk '/[Ss]crub started/ {sub(".*scrub started at ", ""); print; exit}')
    [[ -z "$last_scrub" ]] && last_scrub="unknown"
  fi

  # Determine status
  local status drives_line
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

  # --- JSON output ---
  if $json_output; then
    local disks_json="[]"
    if $verbose; then
      disks_json=$(status_disks_json "$fi_show" "$mount_point")
    fi

    jq -n \
      --argjson schema_version 1 \
      --arg mount_point "$mount_point" \
      --arg status "$status" \
      --argjson total_devices "$total_devices" \
      --argjson present_count "$present_count" \
      --argjson missing_count "$missing_count" \
      --arg profile "$profile" \
      --argjson total_bytes "${total_bytes:-0}" \
      --argjson used_bytes "${used_bytes:-0}" \
      --argjson free_bytes "${free_bytes:-0}" \
      --arg last_scrub "$last_scrub" \
      --argjson disks "$disks_json" \
      '{
        schema_version: $schema_version,
        mount_point: $mount_point,
        status: $status,
        total_devices: $total_devices,
        present_count: $present_count,
        missing_count: $missing_count,
        profile: $profile,
        capacity: {
          total_bytes: $total_bytes,
          used_bytes: $used_bytes,
          free_bytes: $free_bytes
        },
        last_scrub: $last_scrub,
        disks: $disks
      }'
    return
  fi

  # --- Human output ---
  echo "Pool:     $mount_point"
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
  if ! $verbose; then
    return
  fi

  local dev_stats
  dev_stats=$(btrfs device stats "$mount_point" 2>&1 || true)

  echo ""
  echo "Disks:"

  # Parse present devices
  local present_mappers present_devids
  mapfile -t present_mappers < <(echo "$fi_show" | awk '/\/dev\/mapper\// {print $NF}' | sed 's|.*/||')
  mapfile -t present_devids < <(echo "$fi_show" | awk '/\/dev\/mapper\// {for(i=1;i<=NF;i++) if($i=="devid") {print $(i+1); break}}')

  for i in "${!present_mappers[@]}"; do
    local mapper="${present_mappers[$i]}"
    local devid="${present_devids[$i]}"

    # Find by-id path from config
    local by_id=""
    while IFS= read -r d; do
      if [[ "$(basename "$d")" == "$mapper" ]]; then
        by_id="$d"
        break
      fi
    done < <(config_disks)

    echo "  $mapper      devid $devid   present"

    if [[ -n "$by_id" ]]; then
      echo "    Device:  $by_id"
      local real_dev
      real_dev=$(readlink -f "$by_id" 2>/dev/null || true)
      if [[ -n "$real_dev" && -b "$real_dev" ]]; then
        local model serial
        model=$(lsblk -ndo MODEL "$real_dev" 2>/dev/null | xargs || true)
        serial=$(lsblk -ndo SERIAL "$real_dev" 2>/dev/null | xargs || true)
        echo "    Model:   ${model:-(unknown)}"
        echo "    Serial:  ${serial:-(unknown)}"
      fi
      local luks_uuid
      luks_uuid=$(cryptsetup luksUUID "$by_id" 2>/dev/null || true)
      echo "    LUKS:    ${luks_uuid:-(unknown)}"
    else
      echo "    Device:  /dev/mapper/$mapper  (not in config)"
    fi

    # Error counters
    local read_errs write_errs corrupt_errs
    read_errs=$(echo "$dev_stats" | awk -v m="$mapper" 'index($0, "[/dev/mapper/"m"].read_io_errs") {print $NF}')
    write_errs=$(echo "$dev_stats" | awk -v m="$mapper" 'index($0, "[/dev/mapper/"m"].write_io_errs") {print $NF}')
    corrupt_errs=$(echo "$dev_stats" | awk -v m="$mapper" 'index($0, "[/dev/mapper/"m"].corruption_errs") {print $NF}')
    echo "    Errors:  read ${read_errs:-0} / write ${write_errs:-0} / corruption ${corrupt_errs:-0}"
    echo ""
  done

  # Missing disks
  if (( missing_count > 0 )); then
    while IFS= read -r d; do
      local disk_basename
      disk_basename=$(basename "$d")
      local found=false
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
    done < <(config_disks)
  fi
}

# Helper: build JSON array of disk info for status --json --verbose
status_disks_json() {
  local fi_show="$1"
  local mount_point="$2"
  local dev_stats
  dev_stats=$(btrfs device stats "$mount_point" 2>&1 || true)

  local present_mappers present_devids
  mapfile -t present_mappers < <(echo "$fi_show" | awk '/\/dev\/mapper\// {print $NF}' | sed 's|.*/||')
  mapfile -t present_devids < <(echo "$fi_show" | awk '/\/dev\/mapper\// {for(i=1;i<=NF;i++) if($i=="devid") {print $(i+1); break}}')

  local result="["
  local first=true

  for i in "${!present_mappers[@]}"; do
    local mapper="${present_mappers[$i]}"
    local devid="${present_devids[$i]}"
    local by_id="/dev/disk/by-id/$mapper"

    local read_errs write_errs corrupt_errs
    read_errs=$(echo "$dev_stats" | awk -v m="$mapper" 'index($0, "[/dev/mapper/"m"].read_io_errs") {print $NF}')
    write_errs=$(echo "$dev_stats" | awk -v m="$mapper" 'index($0, "[/dev/mapper/"m"].write_io_errs") {print $NF}')
    corrupt_errs=$(echo "$dev_stats" | awk -v m="$mapper" 'index($0, "[/dev/mapper/"m"].corruption_errs") {print $NF}')

    $first || result+=","
    first=false
    result+=$(jq -n \
      --arg mapper "$mapper" \
      --arg by_id "$by_id" \
      --arg devid "$devid" \
      --arg status "present" \
      --argjson read_errs "${read_errs:-0}" \
      --argjson write_errs "${write_errs:-0}" \
      --argjson corrupt_errs "${corrupt_errs:-0}" \
      '{mapper: $mapper, by_id: $by_id, devid: $devid, status: $status, errors: {read: $read_errs, write: $write_errs, corruption: $corrupt_errs}}')
  done

  result+="]"
  echo "$result"
}

# ============================================================================
# Dispatcher
# ============================================================================

# Parse global options before subcommand
while [[ $# -gt 0 ]]; do
  case "$1" in
    --config)
      CONFIG_FILE="$2"
      shift 2
      ;;
    plan|apply|status)
      break
      ;;
    -h|--help|help)
      usage
      ;;
    *)
      # Could be a subcommand flag, break and let subcommand handle it
      break
      ;;
  esac
done

if [[ $# -eq 0 ]]; then
  usage
fi

SUBCOMMAND="$1"
shift

case "$SUBCOMMAND" in
  plan)   cmd_plan "$@" ;;
  apply)  cmd_apply "$@" ;;
  status) cmd_status "$@" ;;
  *)      die "Unknown command: $SUBCOMMAND. Run 'braid --help' for usage." ;;
esac
