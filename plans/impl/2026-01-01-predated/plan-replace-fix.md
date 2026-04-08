# Migration Plan: Enable `braid replace --old` for Live Disk Targets

## Summary
Expand `braid replace` from "dead/missing old disk only" to "live or missing old disk," while preserving add-first ordering and strict safety checks. Live target eviction must be implemented as a shared primitive reused by both `remove` and `replace`.

## Public Interface / Behavior Changes
1. `braid replace --old <name> --new <name>` now accepts:
- live `--old` disk currently in pool, or
- missing/dead old disk (current behavior).
2. `--missing-id` remains valid only for missing/dead path.
3. New validation errors:
- live old + missing devices present: reject with guidance to run `braid remove-missing` first.
- live old + `--missing-id`: reject as incompatible flags.
- `--old == --new`: reject.
4. Confirmation behavior:
- default confirm phrase remains `yes`.
- if operation will end with a single-device pool, require `replace without redundancy`.

## Implementation Plan

### 1. Shared present-device eviction helper
Files: `cli/src/remove.rs`, `cli/src/pool.rs`

Extract the conversion + remove + LUKS-close sequence from `remove` into a shared helper. Both `remove` and `replace` call this helper for live-device eviction.

Helper contract:
- Takes target mapper, mount point, progress output.
- Probes current pool state to decide if conversion is needed (don't accept caller-provided projected counts).
- If removal would leave one device: `pool_balance_single` before remove.
- `pool_remove_device` for the target mapper.
- `cryptsetup close` for the target mapper (best-effort; warn on failure, don't fail command).
- Idempotent: if target mapper is already absent, return `AlreadyAbsent`. Otherwise return `Removed`.

`remove` must switch to this helper (replace its existing inline logic).

### 2. Replace target resolution refactor
File: `cli/src/replace.rs`

1. Extend `EvictionTarget` enum:
- `Live { mapper: MapperName }` (new)
- `Devid(u64)` (existing)
- `Missing` (existing)
2. Replace the early "live old → refuse" branch with resolver logic:
- If old mapper found in pool:
  - if `missing_id.is_some()`: reject.
  - if `pool.missing_count > 0`: reject (mixed state).
  - else `EvictionTarget::Live`.
- Else: existing missing-target resolution.
3. `--old` validated by pool membership only (no config requirement; disk-map remains advisory and is not used for resolution). `--new` config requirement unchanged.
4. Add `old_name == new_name` rejection early.

### 3. Execution flow and checkpoint changes
Files: `cli/src/replace.rs`, `cli/src/checkpoint.rs`

Pre-eviction path unchanged (probe, LUKS, device add, checkpoint at `ReplaceBalanceRaid1`, balance RAID1).

Eviction phase dispatch:
- `Live`: write `Phase::ReplaceEvictLive`, call shared helper.
- `Devid`: write `Phase::ReplaceEvictDead`, existing `pool_remove_devid` path.
- `Missing`: write `Phase::ReplaceEvictDead`, existing `pool_remove_missing` path.

Resume gate for `ReplaceEvictLive` only:
- Skip strict `pool_fingerprint` equality (device list legitimately changes after eviction).
- Allow `secondary_target_available = false` (target may already be gone).
- All other phases retain strict checks.

Checkpoint schema version unchanged.

### 4. Dry-run and confirmation messaging
File: `cli/src/replace.rs`

1. Live path dry-run: show `btrfs device remove /dev/mapper/braid-<old>`. When projected remaining is 1, include conversion step before remove.
2. Remove "dead/new" wording in generic prompt. Preserve LUKS destructive warning.
3. If projected final pool has one device, require `replace without redundancy`.

### 5. CLI help text
File: `cli/src/main.rs`

1. Subcommand description: neutral wording (not "dead disk").
2. `--old` help text: "Name of the disk to replace."

## Test Plan

### 1. Unit tests (Rust)
Add tests for:
1. live old resolution succeeds when no missing devices.
2. live old + `--missing-id` rejects.
3. live old + pool missing devices rejects.
4. `old == new` rejects.
5. dry-run steps: mapper remove for live path; RAID1→single conversion when remaining is 1.
6. confirmation text no longer claims old is dead.
7. no-redundancy confirmation trigger for single-device final topology.
8. shared helper: conversion before remove when remaining is 1; skips when > 1.
9. shared helper: best-effort `cryptsetup close`; warning-only on failure.
10. resume gate relaxes fingerprint + secondary_target for `ReplaceEvictLive`; strict for all other phases.

### 2. Existing integration tests
- `tests/7-replace-failed-disk.nix` / `replace-failed-disk.py`: keep as dead-path regression. Update wording where it implies dead-only.
- `tests/9-braid-remove-disk.nix` / `braid-remove-disk.py`: run as regression for shared helper refactor.

### 3. New live-replace integration test
Files: `tests/26-replace-live-disk.nix`, `tests/replace-live-disk.py`
Update: `flake.nix` checks to include `replace-live-disk`.

Scenarios:
1. Healthy RAID1 pool, live replace disk2 → disk3. Assert: disk2 removed, disk3 present, no missing devices, RAID profile valid, data preserved.
2. Single-device final topology: start from 2-device pool where replace ends with 1 device. Assert conversion honored, single-profile semantics, no data loss.
3. Mixed-state rejection: simulate missing device, attempt live replace, assert failure guidance.
4. `--missing-id` rejection on live path.

### 4. Checkpoint resume coverage
File: `tests/braid-checkpoint-opstate.py`

Add subtests:
1. dead/missing replace interruption/resume succeeds.
2. live replace interruption/resume succeeds (without conversion).
3. live replace interruption/resume succeeds (with conversion-before-evict).
4. idempotent: live-evict resume when target already absent.

### 5. Test file convention
All new test files must start with block comment: what is tested, why it exists, dependencies.

## Docs Updates

### README.md
1. Rename "Replace a failed drive" to broader wording.
2. Document live swap and dead/missing modes.
3. Document mixed-state rejection and `--missing-id` scoping.
4. Clarify redundancy semantics (add-first, final count, explicit confirmation for single-device).

### docs/decisions/012-intent-cli.md
1. Command purpose: "evict target disk (live or missing)."
2. Add mixed-state rejection rule and `--missing-id` scoping.
3. Record shared eviction constraint.

### docs/principles.md
1. Update minimally for live+dead semantics.
2. Keep safe-by-construction intent explicit.

## Acceptance Criteria
1. `braid replace` succeeds for live old disk in healthy pool.
2. `braid replace` succeeds for live old in single-device final topology.
3. dead/missing replace behavior intact.
4. mixed state (live old + missing) fails with actionable message.
5. live old + `--missing-id` fails with actionable message.
6. `old == new` fails with actionable message.
7. checkpoint resume works after interruption, including conversion-before-evict.
8. live replace fully releases old disk (removal + best-effort LUKS close).
9. docs match actual behavior.
10. Full test suite passes (`replace-failed-disk`, `replace-live-disk`, `braid-checkpoint-opstate`, `braid-remove-disk`, Rust unit tests).
