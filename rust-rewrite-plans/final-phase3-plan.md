# Phase 3: Planner — Pure Logic + Golden Tests

## Context

Phase 2 (parse layer) is complete: 10 parsers, typed output structs, 40 tests passing. The planner is the core decision engine that computes what actions braid should take to reconcile config vs. live state. It's pure logic — no I/O, no CommandRunner calls. Takes typed inputs, returns `PlanOutcome`.

The hard rule from 5-plan.md: **define the canonical output contract first, then implement to that spec.** The bash implementation is reference, not law.

Probe/identity modules (the I/O glue that *produces* planner inputs from real commands) are deferred to Phase 3.5. Phase 3 tests construct all inputs by hand.

## Files to modify

- `cli/Cargo.toml` — add `time = { version = "0.3", features = ["formatting", "macros"] }`
- `cli/src/types.rs` — add planner input types (`PoolState`, `PoolDevice`, `ConfigDisk`, `ConfigDiskState`, `PlanFlags`), add `PlanStatus` enum, `PlanSummary` + `PlanReport` for JSON output, add `WarningCode` + `BlockedReasonCode` enums
- `cli/src/plan.rs` — **NEW** — `compute_plan()`, `generate_plan_id()`, `to_plan_report()`, `format_plan_human()`
- `cli/src/lib.rs` — add `pub mod plan`

## Steps

### 1. Add planner input types to `types.rs`

These are the "contract" between probe (future) and planner:

```rust
/// What we know about the live btrfs pool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolState {
    pub mounted: bool,
    pub devices: Vec<PoolDevice>,
    pub missing_count: u64,
    pub total_devices: u64,   // present + missing
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolDevice {
    pub mapper: MapperName,
    pub luks_uuid: LuksUuid,
    pub devid: u64,
}

/// Pre-probed state of each config disk (produced by probe, consumed by planner).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigDisk {
    pub by_id_path: ByIdPath,
    pub state: ConfigDiskState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigDiskState {
    /// Device file doesn't exist (unplugged / absent).
    Absent,
    /// Device exists but is not LUKS-formatted.
    PresentNotLuks,
    /// Device exists, has LUKS header, UUID known.
    /// `mapper_open` = true if /dev/mapper/<name> is already active (crash recovery skip).
    PresentLuks { uuid: LuksUuid, mapper_open: bool },
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PlanFlags {
    pub allow_remove_missing: bool,
    pub allow_remove_ambiguous: bool,
}
```

### 2. Add enums, JSON output types to `types.rs`

**Warning and blocked-reason codes as enums** — used internally for compile-time safety, serialized as strings for JSON output:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum WarningCode {
    DiskAbsentSkipped,
    InitRequired,
    PoolDegradedMissingDevices,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum BlockedReasonCode {
    IdentityAmbiguousAbsentDisk,
    AmbiguousMissing,
}
```

Update `Warning.code` from `String` to `WarningCode`, `BlockedReason.code` from `String` to `BlockedReasonCode`. This replaces string constants with exhaustive enum matching across all tests.

**Migration impact:** `Warning` and `BlockedReason` in `types.rs` currently use `code: String`. Changing to enum types will require updating existing Phase 1 test code in `types.rs` that constructs `BlockedReason { code: "X".to_owned(), .. }` — these become `BlockedReason { code: BlockedReasonCode::..., .. }`. No Phase 2 parser code uses these types, so parse tests are unaffected.

**Plan status as an enum** — not a raw string:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    Applicable,
    ApplicableWithWarnings,
    Blocked,
}
```

**JSON output wrapper types:**

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlanSummary {
    pub actions_total: usize,
    pub actions_mutation: usize,
    pub actions_verify: usize,
    pub warnings_total: usize,
    pub blocked_total: usize,
    pub skipped_total: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlanReport {
    pub schema_version: u32,
    pub plan_id: String,
    pub mount_point: String,
    pub status: PlanStatus,
    pub warning_count: usize,
    pub warnings: Vec<Warning>,
    pub blocked_reasons: Vec<BlockedReason>,
    pub confirmations: Vec<Confirmation>,
    pub actions: Vec<Action>,
    pub summary: PlanSummary,
}
```

### 3. Create `cli/src/plan.rs` — compute_plan()

Signature:
```rust
pub fn compute_plan(
    config: &Config,
    config_disks: &[ConfigDisk],
    pool: &PoolState,
    flags: &PlanFlags,
) -> PlanOutcome
```

**Algorithm (matches bash `compute_plan()` lines 193–564, ported to typed Rust):**

1. **Classify config disks → disks to add:**
   For each `ConfigDisk`:
   - `Absent` → warning `DISK_ABSENT_SKIPPED`, skip
   - `PresentNotLuks` → warning `INIT_REQUIRED`, skip
   - `PresentLuks { uuid, mapper_open }`:
     - UUID already in pool → no-op
     - UUID not in pool → add to `disks_to_add`
       - if `!mapper_open` → emit `OPEN_LUKS` action
       - always emit `ADD_DISK_BTRFS_ADD` action

2. **Identify disks to remove (pool devices not in config):**
   Only when `pool.mounted`. For each pool device, check if its `luks_uuid` matches any config disk UUID. If not → emit `REMOVE_DISK_GRACEFUL` + `CLOSE_LUKS_MAPPER`.

3. **Identity ambiguity check:**
   If removals pending AND any config disk is `Absent`:
   - Without `--allow-remove-ambiguous`: block with `IDENTITY_AMBIGUOUS_ABSENT_DISK` (one reason per absent disk)
   - With flag: add confirmation `"remove despite ambiguous identity"` on first `REMOVE_DISK_GRACEFUL`

4. **Missing device handling** (only when mounted, `missing_count > 0`):
   - Always warn `POOL_DEGRADED_MISSING_DEVICES`
   - If `--allow-remove-missing`:
     - `missing_count > 1` → block with `AMBIGUOUS_MISSING`
     - `missing_count == 1` → emit `REMOVE_DISK_MISSING_EXPLICIT`

5. **BALANCE_TO_RAID1:**
   If adding disks AND future pool size ≥ 2 (accounting for adds, removes, and explicit missing removal) → emit `BALANCE_TO_RAID1`

6. **Confirmations:**
   - `REMOVE_DISK_MISSING_EXPLICIT` present → confirmation `"remove missing device from pool"`
   - Graceful removals AND future pool size < 2 → confirmation `"remove this disk without redundancy"`

7. **Verify actions** (only when mutation actions exist):
   Append `VERIFY_POOL_HEALTH` + `VERIFY_EXPECTED_DISK_SET`. No-op plans produce zero actions.

8. **Assemble:**
   - `blocked_reasons` non-empty → `PlanOutcome::Blocked`
   - Otherwise → `PlanOutcome::Applicable`

**Action emission order:**
1. `OPEN_LUKS` + `ADD_DISK_BTRFS_ADD` (per disk to add)
2. `REMOVE_DISK_GRACEFUL` + `CLOSE_LUKS_MAPPER` (per disk to remove)
3. `REMOVE_DISK_MISSING_EXPLICIT` (if applicable)
4. `BALANCE_TO_RAID1` (if applicable)
5. `VERIFY_POOL_HEALTH`
6. `VERIFY_EXPECTED_DISK_SET`

### 4. `generate_plan_id()` in plan.rs

```rust
pub fn generate_plan_id() -> String              // public API — calls now_utc + new_v4
fn build_plan_id(now: OffsetDateTime, nonce: &str) -> String  // testable internal
```

Format: `{ISO8601_UTC}-{6_hex_chars}` matching bash (e.g. `2026-02-23T14:30:45Z-a1b2c3`).

`build_plan_id` is the deterministic core: formats timestamp, sha256-hashes `"{ts}-{nonce}"`, takes first 6 hex chars. `generate_plan_id` wraps it with real clock + `Uuid::new_v4()`. Tests call `build_plan_id` directly with fixed inputs to assert exact format without flakiness.

Add `time = { version = "0.3", features = ["formatting", "macros"] }` to `Cargo.toml`. Regenerate `Cargo.lock`.

### 5. `to_plan_report()` in plan.rs

```rust
pub fn to_plan_report(outcome: &PlanOutcome, config: &Config) -> PlanReport
```

Takes `&Config` instead of `mount_point: &str` — single source of truth, avoids mismatched paths. Converts `PlanOutcome` → `PlanReport` with computed `summary`. `skipped_total` derived from warning codes (`DiskAbsentSkipped`, `InitRequired`).

### 6. `format_plan_human()` in plan.rs

```rust
pub fn format_plan_human(report: &PlanReport) -> String
```

Human-readable output matching bash `format_plan_human()` (lines 570–646):
```
Plan ID: 2026-02-23T14:30:45Z-abc123
Mount:   /mnt/storage
Status:  applicable
Actions: 2

[1] OPEN_LUKS                      target=/dev/disk/by-id/...
[2] ADD_DISK_BTRFS_ADD             target=/dev/mapper/...

Warnings: none
```

Key: human output prints `"applicable with warnings"` (spaces, readable). JSON serializes as `"applicable_with_warnings"` (snake_case from serde).

### 7. Tests — one per scenario

All in `plan.rs` `#[cfg(test)]` module. Each test constructs `Config`, `Vec<ConfigDisk>`, `PoolState`, `PlanFlags` by hand.

**Test helper functions:**
```rust
fn pool_2disk() -> PoolState { /* 2-disk RAID1, mounted, no missing */ }
fn pool_unmounted() -> PoolState { /* not mounted */ }
fn config_disk_present(path: &str, uuid: &str) -> ConfigDisk { ... }
fn config_disk_absent(path: &str) -> ConfigDisk { ... }
fn config_disk_not_luks(path: &str) -> ConfigDisk { ... }
```

| # | Test name | Scenario | Key assertions |
|---|-----------|----------|----------------|
| 1 | `plan_noop` | Config matches pool | `Applicable`, 0 actions total (verify not appended without mutations) |
| 2 | `plan_add_single_disk` | 1 new LUKS disk, 2-disk pool | `Applicable`, OPEN_LUKS + ADD_DISK_BTRFS_ADD + BALANCE_TO_RAID1 |
| 3 | `plan_add_skip_open_when_mapper_open` | New disk with `mapper_open: true` | No OPEN_LUKS, only ADD_DISK_BTRFS_ADD |
| 4 | `plan_remove_single_disk` | Pool has disk not in config | `Applicable`, REMOVE_DISK_GRACEFUL + CLOSE_LUKS_MAPPER |
| 5 | `plan_replace_disk` | Add one + remove another | Both add and remove actions present |
| 6 | `plan_absent_disk_skip_with_warning` | Config disk absent, no removals | `Applicable`, warning `DISK_ABSENT_SKIPPED` |
| 7 | `plan_init_required_warning` | Config disk present but not LUKS | `Applicable`, warning `INIT_REQUIRED` |
| 8 | `plan_absent_blocks_removal` | Absent disk + pool device to remove | `Blocked`, reason `IDENTITY_AMBIGUOUS_ABSENT_DISK` |
| 9 | `plan_absent_unblocked_with_flag` | Same + `allow_remove_ambiguous` | `Applicable`, confirmation `"remove despite ambiguous identity"` |
| 10 | `plan_multiple_confirmations` | Ambiguity + redundancy loss | `Applicable`, 2 confirmations |
| 11 | `plan_missing_device_warn_only` | Pool has 1 missing, no flag | `Applicable`, warning `POOL_DEGRADED_MISSING_DEVICES`, no remove action |
| 12 | `plan_missing_device_explicit_removal` | 1 missing + `allow_remove_missing` | `Applicable`, REMOVE_DISK_MISSING_EXPLICIT + confirmation |
| 13 | `plan_multiple_missing_blocked` | 2 missing + `allow_remove_missing` | `Blocked`, reason `AMBIGUOUS_MISSING` |
| 14 | `plan_redundancy_confirmation` | Remove to single disk | `Applicable`, confirmation `"remove this disk without redundancy"` |
| 15 | `plan_bootstrap_unmounted` | Pool not mounted, 1 config disk | `Applicable`, OPEN_LUKS + ADD_DISK_BTRFS_ADD, no BALANCE (single disk) |
| 16 | `plan_bootstrap_two_disks` | Pool not mounted, 2 config disks | `Applicable`, adds for both + BALANCE_TO_RAID1 |
| 17 | `plan_no_format_action_exists` | Any scenario | Property: no action has type matching format (enforced by ActionType enum having no format variant) |
| 18 | `plan_blocked_not_convertible` | Blocked plan | `ApplicablePlan::try_from()` returns Err (already tested in types.rs, but confirm integration) |
| 19 | `plan_report_json_schema` | Any applicable plan | `to_plan_report()` produces correct `schema_version`, `summary` counts, `status` is `PlanStatus::Applicable` |
| 20 | `plan_report_skipped_total` | Absent + init-required warnings | `summary.skipped_total` counts both DISK_ABSENT_SKIPPED and INIT_REQUIRED |
| 21 | `plan_human_output_format` | Applicable with warnings | `format_plan_human()` contains "Plan ID:", "Status: applicable with warnings" (human-readable spaces), action lines |

### 8. Update `lib.rs`

Add `pub mod plan;`

### 9. Build and verify

```bash
cd cli && cargo test
```

All Phase 1 + Phase 2 + Phase 3 tests must pass.

## Key decisions

- **Planner is pure logic** — no I/O, no CommandRunner. Takes typed inputs, returns PlanOutcome. Probe.rs (the I/O glue) is deferred to Phase 3.5.
- **Input types defined in `types.rs`** — `PoolState`, `ConfigDisk`, etc. are shared contracts used by both probe (later) and plan.
- **`ConfigDiskState::PresentLuks` includes `mapper_open`** — allows planner to skip OPEN_LUKS for crash recovery (bash line 295).
- **Action ordering matches bash** — adds → removes → missing removal → balance → verify.
- **Verify actions only on mutation** — no-op plans produce zero actions. Health checks belong in `braid status`, not hidden in no-op plans. Simpler model: "actions are work to perform."
- **`PlanReport` wraps `PlanOutcome`** — adds schema_version, mount_point, summary for JSON output. `skipped_total` computed from warning codes.
- **`PlanStatus` enum** — `Applicable`, `ApplicableWithWarnings`, `Blocked`. Serializes to `snake_case`. No raw strings for status.
- **Warning/blocked codes are enums** — `WarningCode` and `BlockedReasonCode` with `#[serde(rename_all = "SCREAMING_SNAKE_CASE")]`. Compile-time exhaustive matching across all tests; serialized as strings in JSON output.
- **No separate `identity.rs` module** — UUID matching is simple enough to inline in the planner (~5 lines). Can extract later if complexity grows.
- **`time` crate for timestamps** — `time = { version = "0.3", features = ["formatting", "macros"] }` added to Cargo.toml. Provides clean ISO 8601 UTC formatting for `generate_plan_id()`. `sha2` + `uuid::Uuid::new_v4()` for the 6-char hash suffix.
- **21 tests** — one per scenario + JSON schema + human output. All construct inputs by hand (no mocking needed).
