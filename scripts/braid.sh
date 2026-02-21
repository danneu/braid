#!/usr/bin/env bash
# braid — unified CLI for braid disk pool management.
# Usage: braid <command> [options]
#   braid init-disk <by-id-path> [--force] [--config <path>]
#   braid plan [--json] [--allow-remove-missing] [--config <path>]
#   braid apply [--resume] [--allow-remove-missing] [--config <path>]
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
  echo "  init-disk  Format a new disk with LUKS (explicit, one-shot)"
  echo "  plan       Preview what actions braid will take"
  echo "  apply      Execute the plan"
  echo "  status     Show pool health"
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
#        $2 = "true" if --allow-remove-missing was passed (optional)
# Outputs: plan JSON to stdout.
compute_plan() {
  local live_state="$1"
  local allow_remove_missing="${2:-false}"
  local mount_point
  mount_point=$(config_mount_point)

  local mounted
  mounted=$(echo "$live_state" | jq -r '.mounted')

  local missing_count=0
  local pool_mappers=()

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

  local pool_backing=()
  if [[ "$mounted" == "true" ]]; then
    missing_count=$(echo "$live_state" | jq -r '.missing_count')
    mapfile -t pool_mappers < <(echo "$live_state" | jq -r '.pool_devices[].mapper')

    # Resolve backing devices for pool mappers via sysfs.
    # Boot-time LUKS may use short mapper names (e.g., "disk1") while braid config
    # uses by-id basenames (e.g., "virtio-disk1"). Comparing underlying kernel
    # device paths lets us detect that they refer to the same physical disk.
    for pm in "${pool_mappers[@]}"; do
      local backing=""
      local dm_path dm_name slave
      dm_path=$(readlink -f "/dev/mapper/$pm" 2>/dev/null || true)
      dm_name=$(basename "$dm_path")
      if [[ -d "/sys/block/$dm_name/slaves" ]]; then
        slave=$(find "/sys/block/$dm_name/slaves" -maxdepth 1 -mindepth 1 -printf '%f\n' 2>/dev/null | head -1)
        if [[ -n "$slave" ]]; then
          backing=$(readlink -f "/dev/$slave")
        fi
      fi
      pool_backing+=("$backing")
    done
  fi

  # --- Disks to ADD: in config but not in pool ---
  local disks_to_add=()
  local blocked_reasons=()
  for i in "${!config_basenames[@]}"; do
    local basename="${config_basenames[$i]}"
    local by_id="${config_disk_map[$i]}"
    local in_pool=false

    for pi in "${!pool_mappers[@]}"; do
      local pm="${pool_mappers[$pi]}"
      if [[ "$pm" == "$basename" ]]; then
        in_pool=true
        break
      fi
      # Different mapper name but same underlying device
      local config_real
      config_real=$(readlink -f "$by_id" 2>/dev/null || true)
      if [[ -n "${pool_backing[$pi]:-}" && -n "$config_real" && "${pool_backing[$pi]}" == "$config_real" ]]; then
        in_pool=true
        break
      fi
    done

    if ! $in_pool; then
      # Check device presence
      if [[ ! -b "$by_id" ]]; then
        # Device absent: skip with warning
        warnings+=("DISK_ABSENT_SKIPPED: $by_id not present; skipping add actions for this disk.")
        continue
      fi

      # Device present — check if LUKS
      if ! cryptsetup isLuks "$by_id" 2>/dev/null; then
        # Device present but not LUKS: needs init-disk first — skip with warning
        warnings+=("INIT_REQUIRED: $by_id is not LUKS formatted. Run: braid init-disk $by_id")
        continue
      fi

      # Device present and LUKS — plan open + add
      disks_to_add+=("$by_id")

      # Check if LUKS is already open (crash recovery)
      local is_open
      is_open=$(echo "$live_state" | jq -r --arg m "$basename" '.open_luks_mappers | index($m) != null')

      if [[ "$is_open" != "true" ]]; then
        actions+=("$(jq -n \
          --arg id "$(next_id)" \
          --arg target "$by_id" \
          '{id: $id, type: "OPEN_LUKS", target: $target, preconditions: ["device_present", "device_is_luks"], status: "pending"}')")
      fi

      actions+=("$(jq -n \
        --arg id "$(next_id)" \
        --arg target "/dev/mapper/$basename" \
        '{id: $id, type: "ADD_DISK_BTRFS_ADD", target: $target, preconditions: ["mapper_open", "not_in_pool"], status: "pending"}')")
    fi
  done

  # --- Disks to REMOVE: in pool but not in config (only when pool is mounted) ---
  local disks_to_remove=()
  if [[ "$mounted" == "true" ]]; then
    for pi in "${!pool_mappers[@]}"; do
      local pm="${pool_mappers[$pi]}"
      local in_config=false
      for ci in "${!config_basenames[@]}"; do
        local cb="${config_basenames[$ci]}"
        if [[ "$cb" == "$pm" ]]; then
          in_config=true
          break
        fi
        # Different mapper name but same underlying device
        local config_real
        config_real=$(readlink -f "${config_disk_map[$ci]}" 2>/dev/null || true)
        if [[ -n "${pool_backing[$pi]:-}" && -n "$config_real" && "${pool_backing[$pi]}" == "$config_real" ]]; then
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

    # --- Missing device handling ---
    if (( missing_count > 0 )); then
      warnings+=("POOL_DEGRADED_MISSING_DEVICES: pool has $missing_count missing device(s) at $mount_point.")

      if [[ "$allow_remove_missing" == "true" ]]; then
        if (( missing_count > 1 )); then
          # Ambiguous: multiple missing — refuse even with explicit gate
          blocked_reasons+=("$(jq -n \
            --arg code "AMBIGUOUS_MISSING" \
            --arg disk "multiple" \
            --arg message "Multiple missing devices detected ($missing_count). Cannot determine which to remove. Resolve manually: btrfs device remove missing $mount_point" \
            '{code: $code, disk: $disk, message: $message}')")
        else
          # Single missing + explicit gate — emit explicit removal action
          actions+=("$(jq -n \
            --arg id "$(next_id)" \
            --arg target "missing" \
            '{id: $id, type: "REMOVE_DISK_MISSING_EXPLICIT", target: $target, preconditions: ["pool_has_missing"], status: "pending"}')")
        fi
      fi
    fi
  fi

  # --- BALANCE_TO_RAID1 if adding disks and pool will have 2+ devices ---
  if (( ${#disks_to_add[@]} > 0 )); then
    local current_pool_size=${#pool_mappers[@]}
    # Only subtract missing_count if explicit removal is active
    local missing_subtract=0
    if [[ "$allow_remove_missing" == "true" ]] && (( missing_count == 1 )); then
      missing_subtract=$missing_count
    fi
    local after_add=$((current_pool_size + ${#disks_to_add[@]} - ${#disks_to_remove[@]} - missing_subtract))
    if (( after_add >= 2 )); then
      actions+=("$(jq -n \
        --arg id "$(next_id)" \
        --arg target "$mount_point" \
        '{id: $id, type: "BALANCE_TO_RAID1", target: $target, preconditions: ["pool_has_2_plus_devices"], status: "pending"}')")
    fi
  fi

  # --- Confirmation for explicit missing-device removal ---
  for a_json in "${actions[@]}"; do
    local atype
    atype=$(echo "$a_json" | jq -r '.type')
    if [[ "$atype" == "REMOVE_DISK_MISSING_EXPLICIT" ]]; then
      local missing_action_id
      missing_action_id=$(echo "$a_json" | jq -r '.id')
      confirmations+=("$(jq -n \
        --arg action_id "$missing_action_id" \
        '{action_id: $action_id, phrase: "remove missing device from pool"}')")
      break
    fi
  done

  # --- Redundancy warning for graceful removes ---
  # (Missing-device removal is already gated by its own confirmation phrase)
  if (( ${#disks_to_remove[@]} > 0 )); then
    local total_with_missing
    total_with_missing=$(echo "$live_state" | jq -r '.total_devices')
    local remove_count=${#disks_to_remove[@]}
    local after_remove=$((total_with_missing - remove_count + ${#disks_to_add[@]}))
    if (( after_remove < 2 )); then
      local remove_action_id=""
      for a_json in "${actions[@]}"; do
        local atype
        atype=$(echo "$a_json" | jq -r '.type')
        if [[ "$atype" == "REMOVE_DISK_GRACEFUL" ]]; then
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

  local blocked_reasons_json="["
  for i in "${!blocked_reasons[@]}"; do
    [[ $i -gt 0 ]] && blocked_reasons_json+=","
    blocked_reasons_json+="${blocked_reasons[$i]}"
  done
  blocked_reasons_json+="]"

  # Determine plan status: blocked if any blocked_reasons exist
  local plan_status="applicable"
  if (( ${#blocked_reasons[@]} > 0 )); then
    plan_status="blocked"
  fi

  # Compute summary counts
  local actions_total=${#actions[@]}
  local actions_mutation=0
  local actions_verify=0
  for a_json in "${actions[@]}"; do
    local atype
    atype=$(echo "$a_json" | jq -r '.type')
    case "$atype" in
      VERIFY_*) actions_verify=$((actions_verify + 1)) ;;
      *) actions_mutation=$((actions_mutation + 1)) ;;
    esac
  done

  local warnings_total=${#warnings[@]}
  local blocked_total=${#blocked_reasons[@]}

  # skipped_total: warning codes representing skipped work
  # (DISK_ABSENT_SKIPPED, INIT_REQUIRED — NOT POOL_DEGRADED_MISSING_DEVICES)
  local skipped_total=0
  for w in "${warnings[@]}"; do
    case "$w" in
      DISK_ABSENT_SKIPPED:*|INIT_REQUIRED:*) skipped_total=$((skipped_total + 1)) ;;
    esac
  done

  jq -n \
    --argjson schema_version 1 \
    --arg plan_id "$plan_id" \
    --arg mount_point "$mount_point" \
    --arg status "$plan_status" \
    --argjson warning_count "$warnings_total" \
    --argjson warnings "$warnings_json" \
    --argjson blocked_reasons "$blocked_reasons_json" \
    --argjson confirmations "$confirmations_json" \
    --argjson actions "$actions_json" \
    --argjson actions_total "$actions_total" \
    --argjson actions_mutation "$actions_mutation" \
    --argjson actions_verify "$actions_verify" \
    --argjson warnings_total "$warnings_total" \
    --argjson blocked_total "$blocked_total" \
    --argjson skipped_total "$skipped_total" \
    '{
      schema_version: $schema_version,
      plan_id: $plan_id,
      mount_point: $mount_point,
      status: $status,
      warning_count: $warning_count,
      warnings: $warnings,
      blocked_reasons: $blocked_reasons,
      confirmations: $confirmations,
      actions: $actions,
      summary: {
        actions_total: $actions_total,
        actions_mutation: $actions_mutation,
        actions_verify: $actions_verify,
        warnings_total: $warnings_total,
        blocked_total: $blocked_total,
        skipped_total: $skipped_total
      }
    }'
}

# ============================================================================
# Plan output formatting
# ============================================================================

format_plan_human() {
  local plan_json="$1"

  local plan_id mount_point plan_status
  plan_id=$(echo "$plan_json" | jq -r '.plan_id')
  mount_point=$(echo "$plan_json" | jq -r '.mount_point')
  plan_status=$(echo "$plan_json" | jq -r '.status')

  local mutation_count
  mutation_count=$(echo "$plan_json" | jq '[.actions[] | select(.type | startswith("VERIFY_") | not)] | length')

  local wc
  wc=$(echo "$plan_json" | jq '.warning_count // (.warnings | length)')
  local status_display="$plan_status"
  if [[ "$plan_status" == "applicable" ]] && (( wc > 0 )); then
    status_display="applicable with warnings"
  fi

  echo "Plan ID: $plan_id"
  echo "Mount: $mount_point"
  echo "Status: $status_display"
  echo "Actions: $mutation_count"
  echo ""

  # Blocked reasons
  local blocked_count
  blocked_count=$(echo "$plan_json" | jq '.blocked_reasons | length')
  if (( blocked_count > 0 )); then
    echo "BLOCKED — apply cannot proceed:"
    echo "$plan_json" | jq -r '.blocked_reasons[].message' | while IFS= read -r msg; do
      echo "  - $msg"
    done
    echo ""
  fi

  if (( mutation_count == 0 )) && (( blocked_count == 0 )); then
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

  if [[ "$plan_status" == "blocked" ]]; then
    echo "Resolve blocked reasons above before running apply."
  else
    echo "Next step: run 'sudo braid apply'"
  fi
}

# ============================================================================
# cmd_init_disk
# ============================================================================

cmd_init_disk() {
  local force=false
  local by_id=""

  while [[ $# -gt 0 ]]; do
    case "$1" in
      --force) force=true; shift ;;
      --config) CONFIG_FILE="$2"; shift 2 ;;
      -h|--help)
        echo "Usage: braid init-disk <by-id-path> [--force] [--config <path>]"
        echo ""
        echo "Format a new disk with LUKS encryption."
        echo "This is a one-shot destructive operation — it will never be"
        echo "called by 'braid plan' or 'braid apply'."
        echo ""
        echo "The disk must be declared in your braid config before init."
        echo ""
        echo "Options:"
        echo "  --force    Re-format a disk that already has a LUKS header."
        echo "             Requires BRAID_CONFIRM='reformat this disk'."
        echo "  --config   Config file (default: /etc/braid/config.json)"
        echo ""
        echo "Environment:"
        echo "  BRAID_PASSPHRASE   Required. Passphrase for LUKS encryption."
        echo "  BRAID_LUKS_OPTS    Optional. Extra options for cryptsetup luksFormat."
        echo "  BRAID_CONFIRM      Required with --force: 'reformat this disk'"
        exit 0
        ;;
      -*)
        die "Unknown option for init-disk: $1"
        ;;
      *)
        if [[ -z "$by_id" ]]; then
          by_id="$1"
        else
          die "Unexpected argument: $1"
        fi
        shift
        ;;
    esac
  done

  if [[ -z "$by_id" ]]; then
    die "Usage: braid init-disk <by-id-path> [--force] [--config <path>]"
  fi

  config_read

  # --- Step 1: Validate by-id path exists and resolves to a block device ---
  if [[ ! -b "$by_id" ]]; then
    die "Device not found or not a block device: $by_id"
  fi

  # --- Step 2: Validate disk is declared in config ---
  local declared=false
  while IFS= read -r disk; do
    if [[ "$disk" == "$by_id" ]]; then
      declared=true
      break
    fi
  done < <(config_disks)

  if ! $declared; then
    die "Disk $by_id is not declared in config ($CONFIG_FILE). Add it to braid.disks first."
  fi

  # --- Step 3: Validate target is not currently part of mounted pool ---
  local mapper_name
  mapper_name=$(basename "$by_id")
  local mount_point
  mount_point=$(config_mount_point)

  if mountpoint -q "$mount_point" 2>/dev/null; then
    local fi_show
    fi_show=$(btrfs filesystem show "$mount_point" 2>/dev/null || true)
    if echo "$fi_show" | grep -q "/dev/mapper/$mapper_name"; then
      die "Disk $by_id is currently part of the mounted pool at $mount_point. Remove it first."
    fi
  fi

  # --- Step 4: Probe LUKS header ---
  if cryptsetup isLuks "$by_id" 2>/dev/null; then
    if ! $force; then
      die "Disk $by_id already has a LUKS header. Use --force to re-format (destructive). Run: braid init-disk $by_id --force"
    fi

    # --force requires confirmation phrase
    local confirm="${BRAID_CONFIRM:-}"
    if [[ "$confirm" != "reformat this disk" ]]; then
      die "--force requires BRAID_CONFIRM='reformat this disk'"
    fi
  fi

  # --- Step 5: Require passphrase ---
  local passphrase="${BRAID_PASSPHRASE:-}"
  if [[ -z "$passphrase" ]]; then
    die "BRAID_PASSPHRASE is required. Export it before running init-disk."
  fi

  # --- Step 6: Enforce single-passphrase invariant ---
  # If pool already has members, verify the passphrase matches an existing member
  local existing_member=""
  while IFS= read -r disk; do
    local dm
    dm=$(basename "$disk")
    if [[ -e "/dev/mapper/$dm" ]] && cryptsetup status "$dm" >/dev/null 2>&1; then
      existing_member="$disk"
      break
    fi
  done < <(config_disks)

  if [[ -z "$existing_member" ]]; then
    # No open members — check if any config disk has a LUKS header we can test against
    while IFS= read -r disk; do
      if [[ "$disk" == "$by_id" ]]; then
        continue
      fi
      if [[ -b "$disk" ]] && cryptsetup isLuks "$disk" 2>/dev/null; then
        existing_member="$disk"
        break
      fi
    done < <(config_disks)
  fi

  if [[ -n "$existing_member" ]]; then
    if ! echo -n "$passphrase" | cryptsetup open --test-passphrase --key-file=- "$existing_member" 2>/dev/null; then
      die "Passphrase does not match existing pool member $existing_member. All disks must use the same passphrase."
    fi
  fi

  # --- Step 7: Run cryptsetup luksFormat ---
  local luks_extra_opts=()
  if [[ -n "${BRAID_LUKS_OPTS:-}" ]]; then
    read -ra luks_extra_opts <<< "$BRAID_LUKS_OPTS"
  fi

  echo "Formatting $by_id with LUKS..."
  echo -n "$passphrase" | cryptsetup luksFormat --batch-mode --key-file=- "${luks_extra_opts[@]}" "$by_id"

  # --- Step 8: Success ---
  echo "LUKS format complete: $by_id"
  echo "Next step: run 'braid apply' to open and add this disk to the pool."
}

# ============================================================================
# cmd_plan
# ============================================================================

cmd_plan() {
  local json_output=false
  local allow_remove_missing=false

  while [[ $# -gt 0 ]]; do
    case "$1" in
      --json) json_output=true; shift ;;
      --allow-remove-missing) allow_remove_missing=true; shift ;;
      --config) CONFIG_FILE="$2"; shift 2 ;;
      *) die "Unknown option for plan: $1" ;;
    esac
  done

  config_read

  local live_state
  live_state=$(discover_live_state)

  local plan
  plan=$(compute_plan "$live_state" "$allow_remove_missing")

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

action_open_luks() {
  local target="$1"  # by-id path
  local mapper_name
  mapper_name=$(basename "$target")

  local passphrase="${BRAID_PASSPHRASE:-}"
  if [[ -z "$passphrase" ]]; then
    die "BRAID_PASSPHRASE is required to open LUKS devices."
  fi

  if [[ ! -b "$target" ]]; then
    die "Device not found: $target"
  fi

  if ! cryptsetup isLuks "$target" 2>/dev/null; then
    die "Device $target is not LUKS formatted. Run: braid init-disk $target"
  fi

  # Already open with expected mapper name — idempotent skip
  if [[ -e "/dev/mapper/$mapper_name" ]] && cryptsetup status "$mapper_name" >/dev/null 2>&1; then
    echo "LUKS device $target already open as $mapper_name — skipping."
    return 0
  fi

  # Device already mapped under a different name (e.g., boot-time LUKS with short names)
  local real_dev
  real_dev=$(readlink -f "$target")
  local dev_basename
  dev_basename=$(basename "$real_dev")
  local holders_dir="/sys/block/$dev_basename/holders"
  if [[ -d "$holders_dir" ]] && [[ -n "$(ls -A "$holders_dir" 2>/dev/null)" ]]; then
    echo "Device $target already in use (mapped under different name) — skipping."
    return 0
  fi

  echo "Opening LUKS device $target as $mapper_name..."
  echo -n "$passphrase" | cryptsetup luksOpen --key-file=- "$target" "$mapper_name"
}

action_btrfs_add() {
  local target="$1"  # mapper path like /dev/mapper/virtio-disk3
  local mount_point
  mount_point=$(config_mount_point)

  # Check if this is the first disk (pool doesn't exist yet) or adding to existing
  local pool_devs=0
  pool_devs=$(btrfs filesystem show "$mount_point" 2>/dev/null | grep -c "devid" || true)
  pool_devs=${pool_devs:-0}

  if (( pool_devs == 0 )); then
    echo "Creating btrfs filesystem on $target..."
    mkfs.btrfs -f "$target"
    mkdir -p "$mount_point"
    mount "$target" "$mount_point"
  else
    # Check if device is already a pool member (returning missing device).
    # If the mapper is already known to btrfs after a device scan, skip add.
    btrfs device scan "$target" 2>/dev/null || true
    local fi_show
    fi_show=$(btrfs filesystem show "$mount_point" 2>/dev/null || true)
    if echo "$fi_show" | grep -q "$target"; then
      echo "Device $target is a returning pool member (was missing). Scan complete."
    else
      echo "Adding $target to btrfs pool..."
      # Use -f to handle devices with stale btrfs metadata (e.g. previously evicted)
      btrfs device add -f "$target" "$mount_point"
    fi
  fi
}

action_balance_raid1() {
  local target="$1"  # mount point
  # Check if already RAID1 — skip if so (handles returning-member case)
  local current_profile
  current_profile=$(btrfs filesystem df "$target" 2>/dev/null | awk '/^Data,/ {sub("^Data, *", ""); sub(":.*", ""); print}')
  if [[ "$current_profile" == "RAID1" ]]; then
    local fi_show
    fi_show=$(btrfs filesystem show "$target" 2>/dev/null || true)
    if ! echo "$fi_show" | grep -qi "missing"; then
      echo "Pool is already RAID1 with no missing devices. Balance not needed."
      return
    fi
  fi

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

  # Check if we need to convert from RAID1 to single first
  local fi_show
  fi_show=$(btrfs filesystem show "$mount_point" 2>/dev/null || true)
  local present_count
  present_count=$(echo "$fi_show" | awk '/\/dev\/mapper\// {c++} END {print c+0}')
  local total_devices
  total_devices=$(echo "$fi_show" | awk '/Total devices/ {for(i=1;i<=NF;i++) if($i=="devices") {print $(i+1); exit}}')
  local remaining=$((present_count))  # after removing missing, only present devices remain

  if (( remaining < 2 )); then
    echo "Converting pool to single profile before removing missing device..."
    btrfs balance start -dconvert=single -mconvert=single -f "$mount_point"
  fi

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
  local allow_remove_missing=false

  while [[ $# -gt 0 ]]; do
    case "$1" in
      --resume) resume=true; shift ;;
      --allow-remove-missing) allow_remove_missing=true; shift ;;
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
    plan_json=$(compute_plan "$live_state" "$allow_remove_missing")

    # Check for blocked plan
    local plan_status
    plan_status=$(echo "$plan_json" | jq -r '.status')
    if [[ "$plan_status" == "blocked" ]]; then
      echo "Apply blocked:" >&2
      echo "$plan_json" | jq -r '.blocked_reasons[].message' | while IFS= read -r msg; do
        echo "  - $msg" >&2
      done
      exit 1
    fi

    # Print warnings from plan
    local warning_count
    warning_count=$(echo "$plan_json" | jq '.warnings | length')
    if (( warning_count > 0 )); then
      echo "Warnings:"
      echo "$plan_json" | jq -r '.warnings[]' | while IFS= read -r w; do
        echo "  - $w"
      done
    fi

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
      local provided="${BRAID_CONFIRM:-}"
      local required_phrases
      mapfile -t required_phrases < <(echo "$plan_json" | jq -r '.confirmations[].phrase')
      for phrase in "${required_phrases[@]}"; do
        if [[ -z "$provided" ]]; then
          die "This operation requires confirmation. Set BRAID_CONFIRM='$phrase'"
        fi
        if [[ "$provided" != "$phrase" ]]; then
          die "Confirmation phrase doesn't match. Expected: '$phrase'"
        fi
      done
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

    # Resume safety: verify target device is present for device-targeting actions
    if $resume; then
      case "$action_type" in
        OPEN_LUKS)
          if [[ ! -b "$action_target" ]]; then
            echo "Error: RESUME_TARGET_MISSING — device $action_target is absent for pending action $action_id ($action_type)." >&2
            echo "Checkpoint preserved. Resolve device availability and retry with --resume." >&2
            exit 1
          fi
          ;;
        ADD_DISK_BTRFS_ADD)
          if [[ ! -e "$action_target" ]]; then
            echo "Error: RESUME_TARGET_MISSING — mapper $action_target is absent for pending action $action_id ($action_type)." >&2
            echo "Checkpoint preserved. Resolve device availability and retry with --resume." >&2
            exit 1
          fi
          ;;
        REMOVE_DISK_GRACEFUL)
          if [[ ! -e "$action_target" ]]; then
            echo "Error: RESUME_TARGET_MISSING — mapper $action_target is absent for pending action $action_id ($action_type)." >&2
            echo "Checkpoint preserved. Resolve device availability and retry with --resume." >&2
            exit 1
          fi
          ;;
      esac
    fi

    echo ""
    echo "[$action_id] $action_type target=$action_target"
    checkpoint_update_action "$action_id" "in_progress"

    case "$action_type" in
      OPEN_LUKS)                     action_open_luks "$action_target" ;;
      ADD_DISK_BTRFS_ADD)            action_btrfs_add "$action_target" ;;
      BALANCE_TO_RAID1)              action_balance_raid1 "$action_target" ;;
      REMOVE_DISK_GRACEFUL)          action_remove_graceful "$action_target" ;;
      REMOVE_DISK_MISSING)           action_remove_missing ;;
      REMOVE_DISK_MISSING_EXPLICIT)  action_remove_missing ;;
      CLOSE_LUKS_MAPPER)             action_close_luks "$action_target" ;;
      VERIFY_POOL_HEALTH)            action_verify_health ;;
      VERIFY_EXPECTED_DISK_SET)      action_verify_diskset ;;
      *) die "Unknown action type: $action_type" ;;
    esac

    checkpoint_update_action "$action_id" "completed"

    # Test hook: simulate failure after completing specific action
    if [[ -n "${BRAID_TEST_FAIL_AFTER_ACTION:-}" && "$action_id" == "$BRAID_TEST_FAIL_AFTER_ACTION" ]]; then
      die "Test hook: simulated failure after action $action_id"
    fi
  done

  # applied: count actually completed mutation actions from checkpoint (execution state)
  local applied
  applied=$(jq '[.actions[] | select(.status == "completed" and (.type | startswith("VERIFY_") | not))] | length' "$CHECKPOINT_FILE")
  # skipped: count from plan_json (plan-level metadata, static during execution)
  local skipped
  skipped=$(echo "$plan_json" | jq '[.warnings[] | select(startswith("DISK_ABSENT_SKIPPED:") or startswith("INIT_REQUIRED:"))] | length')

  echo ""
  echo "Applied $applied actions, skipped $skipped with warnings, blocked 0"
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
    init-disk|plan|apply|status)
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
  init-disk) cmd_init_disk "$@" ;;
  plan)      cmd_plan "$@" ;;
  apply)     cmd_apply "$@" ;;
  status)    cmd_status "$@" ;;
  *)         die "Unknown command: $SUBCOMMAND. Run 'braid --help' for usage." ;;
esac
