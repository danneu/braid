# Phase 5: `braid status` + CLI Wire-Up

## Context

The Rust CLI has `plan` and `apply` fully implemented and passing VM tests. `braid status` is stubbed (`main.rs:136` prints "not yet implemented"). All required parsers, probe functions, and command runner infrastructure exist. This phase implements the status subcommand and wires it into the binary.

## Output contract

Match the bash implementation's output format exactly. The VM test (`tests/braid-status.py`) defines the assertions.

**Human (default)**:
```
Pool:     /mnt/storage
Status:   healthy
Drives:   3
Profile:  RAID1

Capacity:
  Total:  XX.XX GiB
  Used:   XX.XX MiB
  Free:   XX.XX GiB

Last scrub: never
```

Degraded: `Status: DEGRADED (N missing device[s])`, `Drives: M present, N missing`

Not mounted: just `Pool:` + `Status: not mounted`

**Verbose** adds per-disk section with `Device:`, `Model:`, `Serial:`, `LUKS:`, `Errors:`, and `MISSING` entries for absent disks.

**JSON** (`--json`): `schema_version: 1`, fields match bash. `disks: []` unless `--verbose`.

## Architecture

```
main.rs  →  status::cmd_status(runner, config, verbose, json)
                ↓
            probe::probe_pool(runner, mount_point) → PoolState
                ↓
            status::gather_status(runner, config, pool, verbose) → StatusReport
                ↓
            JSON: serde_json::to_string_pretty(&report)
            Human: format_status_human(&report)
```

`status.rs` calls existing parsers for additional data (`btrfs df`, `usage`, `scrub status`, `device stats`, `lsblk field`). No raw string parsing in domain code.

## Steps

### 1. Create `cli/src/status.rs`

**Types** (serde-serializable for JSON output):

```rust
#[derive(Serialize)]
pub struct StatusReport {
    pub schema_version: u32,            // always 1
    pub mount_point: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_devices: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub present_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub missing_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capacity: Option<CapacityReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_scrub: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub disks: Vec<DiskReport>,
}

#[derive(Serialize)]
pub struct CapacityReport {
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub free_bytes: u64,
}

#[derive(Serialize)]
pub struct DiskReport {
    pub mapper: String,
    pub by_id: String,
    pub luks_uuid: String,
    pub devid: Option<String>,
    pub status: String,       // "present" | "missing"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub serial: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub errors: Option<DiskErrors>,
}

#[derive(Serialize)]
pub struct DiskErrors {
    pub read: u64,
    pub write: u64,
    pub corruption: u64,
}
```

**Error type:**

```rust
#[derive(Debug, thiserror::Error)]
pub enum StatusError {
    #[error("probe error: {0}")]
    Probe(#[from] ProbeError),
    #[error("json serialization failed: {0}")]
    Json(#[from] serde_json::Error),
}
```

**Public functions:**

- `cmd_status<R: CommandRunner>(runner: &R, config: &Config, verbose: bool, json: bool) -> Result<(), StatusError>` — orchestrator
- `gather_status<R: CommandRunner>(runner: &R, config: &Config, pool: &PoolState, verbose: bool) -> StatusReport` — pure data collection (infallible after probe; command failures → "unknown" fallbacks)
- `format_status_human(report: &StatusReport) -> String` — formatting only

**Private helpers:**

- `format_bytes(bytes: u64) -> String` — TiB/GiB/MiB/KiB/B thresholds, `%.2f` format
- `scrub_string(runner, mount_point) -> String` — runs scrub status, returns "never"/"unknown"/timestamp (swallows all errors)
- `profile_string(runner, mount_point) -> String` — runs df json, finds Data entry's `bg_profile`, falls back to "unknown"

**Verbose disk-mapping logic:**

1. Run `BtrfsDeviceStats` → parse → `Vec<DeviceErrorStats>`
2. For each `PoolDevice`: find matching config disk by `luks_uuid == ConfigDisk.PresentLuks.uuid`, get `by_id_path`. Run `LsblkField { device: by_id_path, field: Model/Serial }` (swallow failures → `None`). Find matching error stats by `device_path == /dev/mapper/{mapper.0}`.
3. For missing disks: iterate config disks; any config disk whose UUID doesn't match a pool device (or is `Absent`) is missing. Emit `MISSING` entry.

**Scrub error handling:** `btrfs scrub status` can exit non-zero on a fresh pool. Strategy: run command, if `CmdError` → "unknown". If exit 0, parse normally. If `ParseError::CommandFailed` → "unknown". Only `ScrubState::Never/Completed/Unknown` propagate.

### 2. Add module to `cli/src/lib.rs`

Add `pub mod status;` after existing modules.

### 3. Wire into `cli/src/main.rs`

Replace `Commands::Status(_) => println!("not yet implemented")` with:

```rust
Commands::Status(args) => {
    let config = match config_read(Path::new(&config_path)) { ... };
    let runner = RealRunner;
    if let Err(e) = braid_cli::status::cmd_status(&runner, &config, args.verbose, args.json) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
```

Add `use braid_cli::status::cmd_status;` (or call qualified).

### 4. Unit tests in `status.rs`

All tests use `MockRunner`. Grouped as:

**Core behavior:**
- `status_not_mounted` — verifies human + JSON output for unmounted pool
- `status_healthy_single` — 1 disk, "single" profile, capacity lines, no "RAID1"/"missing"
- `status_healthy_raid1` — 3 disks, "RAID1", capacity, scrub line
- `status_degraded` — "DEGRADED (1 missing device)", "2 present, 1 missing"
- `status_degraded_plural` — "DEGRADED (2 missing devices)"

**JSON:**
- `status_json_not_mounted` — parse JSON, verify `schema_version`, `status`, no capacity fields
- `status_json_healthy` — all fields present, `disks: []` (non-verbose)
- `status_json_verbose_disks` — `disks` array populated with present disk entries

**Verbose:**
- `status_verbose_present_disks` — each disk shown with "present", "devid", "LUKS:", "Errors:"
- `status_verbose_missing_disk` — "MISSING", config disk name, "not found" or "device absent"
- `status_verbose_lsblk_failure` — model/serial unavailable → "(unknown)", not fatal

**Edge cases:**
- `status_scrub_completed` — timestamp appears in output
- `status_scrub_failure_is_unknown` — command error → "unknown", not fatal
- `format_bytes_units` — B/KiB/MiB/GiB/TiB boundaries

### 5. NixOS VM test

**`tests/18-braid-status-rust.nix`** — follows `15-braid-plan-rust.nix` pattern:
- Takes `{ braid-rust }:` argument
- 3 virtual disks (disk1, disk2, disk3) at 1024 MiB
- Includes both bash `braid` (for setup) and `braid-rust` (for status testing)
- Config: 3 disks at `/dev/disk/by-id/virtio-disk{1,2,3}`, mount `/mnt/storage`

**`tests/braid-status-rust.py`** — mirrors `tests/braid-status.py` but uses `braid-rust status`:
- Same 4 phases: single-disk, RAID1 healthy, degraded, unmounted
- Same assertions, `braid-rust status` instead of `braid status`

**`flake.nix`** — add after `braid-apply-rust`:
```nix
braid-status-rust = pkgs.testers.nixosTest (import ./tests/18-braid-status-rust.nix {
  braid-rust = linuxCrane.braid-rust;
});
```

## Reused infrastructure

| What | Where |
|------|-------|
| `probe_pool()` | `cli/src/probe.rs:87` |
| `probe_config_disk()` | `cli/src/probe.rs:44` |
| `mapper_name_for_by_id()` | `cli/src/plan.rs:465` (pub(crate)) |
| `parse_btrfs_df_json()` | `cli/src/parse/btrfs_filesystem_df.rs` |
| `parse_btrfs_filesystem_usage()` | `cli/src/parse/btrfs_filesystem_usage.rs` |
| `parse_btrfs_scrub_status()` | `cli/src/parse/btrfs_scrub_status.rs` |
| `parse_btrfs_device_stats()` | `cli/src/parse/btrfs_device_stats.rs` |
| `parse_lsblk_field()` | `cli/src/parse/lsblk.rs` |
| `MockRunner` | `cli/src/cmd.rs:216` |
| `CmdRequest::*` | `cli/src/cmd.rs:18` (all variants exist) |
| `Config`, `config_read()` | `cli/src/config.rs` |
| `PoolState`, `PoolDevice`, `ConfigDisk` | `cli/src/types.rs` |
| `ScrubState`, `BtrfsDfOutput`, etc. | `cli/src/parse/types.rs` |

## Files modified

| File | Change |
|------|--------|
| `cli/src/status.rs` | **New** — core implementation + unit tests |
| `cli/src/lib.rs` | Add `pub mod status;` |
| `cli/src/main.rs` | Wire `Commands::Status` to `cmd_status` |
| `tests/18-braid-status-rust.nix` | **New** — VM test config |
| `tests/braid-status-rust.py` | **New** — VM test assertions |
| `flake.nix` | Register `braid-status-rust` check |

## Verification

1. `cargo test -p braid-cli` — all unit tests pass (existing + new status tests)
2. `cargo test -p braid-cli -- status` — status tests specifically
3. `make test-one t=braid-status-rust` — VM integration test
4. `make test` — full suite still passes
