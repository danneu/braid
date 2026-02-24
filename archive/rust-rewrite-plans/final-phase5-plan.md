# Phase 5: `braid status` + CLI Wire-Up

## Context

The Rust CLI has `plan` and `apply` fully implemented and passing VM tests. `braid status` is stubbed (`main.rs:136` prints "not yet implemented"). All required parsers, probe functions, and command runner infrastructure exist. This phase implements the status subcommand and wires it into the binary.

## Contract direction

**Better-than-bash, intentional divergence.** The Rust implementation defines a stable JSON schema with typed status values and an explicit error policy. Human output matches bash test assertions; JSON output improves on bash by providing a stable envelope with always-present keys.

---

## JSON schema contract (stable envelope)

**Always-present keys** (in every response, all states):

```json
{
  "schema_version": 1,
  "mount_point": "/mnt/storage",
  "status_code": "healthy",
  "status": "healthy",
  "disks": []
}
```

**Mounted-only keys** (present only when `status_code != "not_mounted"`):

```json
{
  "total_devices": 3,
  "present_count": 3,
  "missing_count": 0,
  "profile": "RAID1",
  "capacity": {
    "total_bytes": 1040187392,
    "used_bytes": 33914880,
    "free_bytes": 442957824
  },
  "last_scrub": "never"
}
```

**No ad-hoc optional fields.** Schema changes require an explicit version bump.

### Status semantics

`status_code` is machine-readable, stable, one of: `healthy`, `degraded`, `not_mounted`.

`status` is display-oriented:
- healthy → `"healthy"`
- degraded → `"DEGRADED (N missing device)"` / `"DEGRADED (N missing devices)"`
- not_mounted → `"not mounted"`

### DiskReport (JSON, only when verbose)

Fields: `mapper`, `by_id`, `luks_uuid`, `devid`, `status`, `errors`.

No `model`/`serial` in JSON. Model/serial are human-verbose only.

```json
{
  "mapper": "virtio-disk1",
  "by_id": "/dev/disk/by-id/virtio-disk1",
  "luks_uuid": "71ff9937-...",
  "devid": "1",
  "status": "present",
  "errors": { "read": 0, "write": 0, "corruption": 0 }
}
```

Missing disk:
```json
{
  "mapper": "virtio-disk3",
  "by_id": "/dev/disk/by-id/virtio-disk3",
  "luks_uuid": "",
  "devid": null,
  "status": "missing",
  "errors": null
}
```

---

## Human output contract

**Default (healthy):**
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

**Degraded:** `Status:   DEGRADED (N missing device[s])`, `Drives:   M present, N missing`

**Not mounted:**
```
Pool:     /mnt/storage
Status:   not mounted
```

**Verbose** appends per-disk section:
```

Disks:

  virtio-disk1      devid 1   present
    Device:  /dev/disk/by-id/virtio-disk1
    Model:   (unknown)
    Serial:  disk1
    LUKS:    71ff9937-...
    Errors:  read 0 / write 0 / corruption 0

  virtio-disk3      MISSING
    Device:  /dev/disk/by-id/virtio-disk3  (not found)
    Errors:  unknown (device absent)
```

---

## Error policy

Explicitly documented. Two categories:

### Fatal (return `StatusError`, exit 1)

| Data source | When needed |
|-------------|-------------|
| `probe_pool` (findmnt + btrfs show) | Always (mounted check) |
| `btrfs filesystem df` (profile) | Mounted path |
| `btrfs filesystem usage --raw` (capacity) | Mounted path |
| `btrfs device stats` (error counters) | Verbose path |
| `probe_config_disk` (config disk state) | Verbose path |

**Special case: `ProbeError::NotBtrfs`** — `probe_pool` returns `NotBtrfs` when the mount point exists but is not btrfs. This is mapped to `status_code = "not_mounted"` (same as unmounted). It is **not fatal**. Implementation: catch `ProbeError::NotBtrfs` explicitly in `cmd_status` before the `?` propagation, treat as unmounted.

### Tolerant (fallback value, never fatal)

| Data source | Fallback |
|-------------|----------|
| `btrfs scrub status` | `last_scrub = "unknown"` |
| `lsblk` model/serial lookup | Human: `(unknown)`, JSON: not in schema |

---

## Architecture

```
main.rs
  → config_read(path)
  → cmd_status(runner, fs, config, verbose, json)

status.rs: cmd_status staged flow:
  1. probe_pool(runner, mount_point) → PoolState  (NotBtrfs → unmounted)
  2. if !pool.mounted → emit not-mounted report (StatusCode::NotMounted), return Ok(())
  3. gather strict mounted data:
     - get_profile(runner, mount_point) → Result<String>
     - get_capacity(runner, mount_point) → Result<CapacityReport>
     - get_scrub_string(runner, mount_point) → String  (tolerant)
  4. if verbose:
     - probe_config_disk(runner, fs, disk) for each config disk → Result<Vec<ConfigDisk>>
     - get_device_stats(runner, mount_point) → Result<BtrfsDeviceStatsOutput>
     - build_disk_reports(runner, config_disks, pool, device_stats) → Vec<DiskReport>
  5. assemble StatusReport
  6. output JSON or human
```

**`main.rs` stays thin** — only reads config, then calls `cmd_status`. Status module owns all orchestration including probing.

---

## Steps

### 1. Create `cli/src/status.rs`

**Types:**

```rust
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusCode {
    Healthy,
    Degraded,
    NotMounted,
}

impl StatusCode {
    pub fn display_status(self, missing_count: u64) -> String {
        match self {
            StatusCode::Healthy => "healthy".to_owned(),
            StatusCode::Degraded if missing_count == 1 =>
                "DEGRADED (1 missing device)".to_owned(),
            StatusCode::Degraded =>
                format!("DEGRADED ({missing_count} missing devices)"),
            StatusCode::NotMounted => "not mounted".to_owned(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct StatusReport {
    pub schema_version: u32,
    pub mount_point: String,
    pub status_code: StatusCode,
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
    pub disks: Vec<DiskReport>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CapacityReport {
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub free_bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiskReport {
    pub mapper: String,
    pub by_id: String,
    pub luks_uuid: String,
    pub devid: Option<String>,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub errors: Option<DiskErrors>,
}

#[derive(Debug, Clone, Serialize)]
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
    #[error("command error: {0}")]
    Cmd(#[from] CmdError),
    #[error("parse error: {0}")]
    Parse(#[from] ParseError),
    #[error("json serialization failed: {0}")]
    Json(#[from] serde_json::Error),
}
```

**Public API:**

```rust
pub fn cmd_status<R: CommandRunner, F: Filesystem>(
    runner: &R,
    fs: &F,
    config: &Config,
    verbose: bool,
    json: bool,
) -> Result<(), StatusError>
```

Staged flow inside `cmd_status`:

1. Probe pool, mapping `NotBtrfs` to not-mounted:
   ```rust
   let pool = match probe_pool(runner, &config.mount_point) {
       Ok(p) => p,
       Err(ProbeError::NotBtrfs { .. }) => PoolState { mounted: false, devices: vec![], missing_count: 0, total_devices: 0 },
       Err(e) => return Err(e.into()),
   };
   ```
2. If `!pool.mounted`:
   - `let code = StatusCode::NotMounted;`
   - Build minimal `StatusReport { status_code: code, status: code.display_status(0), disks: vec![], ... (all mounted-only fields None) }`
   - Emit and return `Ok(())`
3. Strict data gathering:
   - `let profile = get_profile(runner, &config.mount_point)?;`
   - `let capacity = get_capacity(runner, &config.mount_point)?;`
   - `let last_scrub = get_scrub_string(runner, &config.mount_point);` (tolerant)
4. Compute `StatusCode`:
   - `pool.missing_count == 0` → `StatusCode::Healthy`
   - `pool.missing_count > 0` → `StatusCode::Degraded`
   - `status = code.display_status(pool.missing_count)`
5. If verbose:
   - `let config_disks = config.disks.iter().map(|d| probe_config_disk(runner, fs, d)).collect::<Result<Vec<_>, _>>()?;`
   - `let device_stats = get_device_stats(runner, &config.mount_point)?;`
   - `let verbose_ctx = build_disk_reports(runner, &config_disks, &pool, &device_stats);` (lsblk tolerant inside)
6. Assemble `StatusReport` (disks from `verbose_ctx.disks` or empty vec)
7. If `json`: `serde_json::to_string_pretty` → stdout
8. Else: `format_status_human(&report, verbose_ctx.as_ref().map(|v| v.human_details.as_slice()))` → stdout

**Chosen approach:** `cmd_status` builds a `VerboseContext` struct when verbose is true:

```rust
struct VerboseContext {
    disks: Vec<DiskReport>,         // for JSON
    human_details: Vec<HumanDisk>,  // for human output (includes model/serial)
}

struct HumanDisk {
    mapper: String,
    by_id: String,
    luks_uuid: String,
    devid: Option<String>,
    status: String,
    model: Option<String>,
    serial: Option<String>,
    errors: Option<DiskErrors>,
}
```

`format_status_human(report: &StatusReport, human_disks: Option<&[HumanDisk]>) -> String` — pure formatting, no I/O.

**Private helpers:**

```rust
// Strict (return Result):
fn get_profile<R: CommandRunner>(runner: &R, mount_point: &str) -> Result<String, StatusError>
fn get_capacity<R: CommandRunner>(runner: &R, mount_point: &str) -> Result<CapacityReport, StatusError>
fn get_device_stats<R: CommandRunner>(runner: &R, mount_point: &str) -> Result<BtrfsDeviceStatsOutput, StatusError>

// Tolerant (never fail):
fn get_scrub_string<R: CommandRunner>(runner: &R, mount_point: &str) -> String
fn get_lsblk_field<R: CommandRunner>(runner: &R, device: &str, field: LsblkFieldKind) -> Option<String>
fn format_bytes(bytes: u64) -> String
```

**`build_disk_reports` logic:**

```rust
fn build_disk_reports<R: CommandRunner>(
    runner: &R,
    config_disks: &[ConfigDisk],
    pool: &PoolState,
    device_stats: &BtrfsDeviceStatsOutput,
) -> VerboseContext
```

1. Build lookup: `pool_uuid_set = pool.devices.iter().map(|d| &d.luks_uuid).collect::<HashSet<_>>()`
2. For each `PoolDevice`:
   - Find matching `ConfigDisk` with `PresentLuks { uuid } where uuid == pool_device.luks_uuid`
   - `by_id` = matched config disk's `by_id_path.0`, or `format!("/dev/mapper/{}", mapper.0)` if not in config
   - Model/serial: `get_lsblk_field(runner, &by_id, Model/Serial)` (tolerant)
   - Errors: find `device_stats.devices` entry where `device_path == format!("/dev/mapper/{}", mapper.0)`
   - Emit present `DiskReport` + `HumanDisk`
3. For each `ConfigDisk` not matched to a pool device:
   - `Absent` → missing (no UUID available)
   - `PresentLuks { uuid }` where uuid not in `pool_uuid_set` → missing
   - `PresentNotLuks` → skip (not a pool member)
   - Mapper name from `mapper_name_for_by_id(by_id_path)`
   - Emit missing `DiskReport` + `HumanDisk` with `luks_uuid: ""`, `devid: None`, `errors: None`

### 2. Add module to `cli/src/lib.rs`

Add `pub mod status;` after existing modules.

### 3. Wire into `cli/src/main.rs`

Replace stub at line 136. main.rs stays thin:

```rust
Commands::Status(args) => {
    let config = match config_read(Path::new(&config_path)) {
        Ok(c) => c,
        Err(e) => { eprintln!("error: {e}"); std::process::exit(1); }
    };
    let runner = RealRunner;
    let fs = RealFilesystem;
    if let Err(e) = braid_cli::status::cmd_status(&runner, &fs, &config, args.verbose, args.json) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
```

Add import: `use braid_cli::probe::RealFilesystem;` (already imported for plan).

### 4. Unit tests in `status.rs`

All tests use `MockRunner` + `MockFs` with inline data. No file fixtures.

**Schema envelope tests:**
- `status_json_not_mounted` — parse JSON as `serde_json::Value`. **Exhaustive key-presence assertions:**
  - Must exist: `schema_version` (== 1), `mount_point`, `status_code` (== "not_mounted"), `status` (== "not mounted"), `disks` (== [])
  - Must NOT exist: `total_devices`, `present_count`, `missing_count`, `profile`, `capacity`, `last_scrub`
  - Assert total key count == 5 (locks the envelope against accidental additions)
- `status_json_healthy` — all mounted-only fields present. `status_code == "healthy"`, `disks == []` (non-verbose).
- `status_json_degraded` — `status_code == "degraded"`, `status` contains "DEGRADED".
- `status_json_verbose_disks` — `disks` array populated. Each present disk has `mapper`, `by_id`, `luks_uuid`, `devid`, `status == "present"`, `errors`. Missing disk has `status == "missing"`, `errors == null`.

**`disks` always-array tests:**
- `status_json_disks_always_array_not_mounted` — `disks` is `[]`
- `status_json_disks_always_array_non_verbose` — `disks` is `[]`
- `status_json_disks_always_array_verbose` — `disks` is non-empty array

**Human output tests:**
- `status_human_not_mounted` — "not mounted", no capacity/profile lines
- `status_human_healthy_single` — "healthy", "Drives:   1", "single", capacity, no "RAID1"/"missing"
- `status_human_healthy_raid1` — "healthy", "Drives:   3", "RAID1", capacity, "scrub"
- `status_human_degraded` — "DEGRADED (1 missing device)", "2 present, 1 missing"
- `status_human_degraded_plural` — "DEGRADED (2 missing devices)"

**Verbose human:**
- `status_verbose_present_disks` — "present", "devid", "LUKS:", "Errors:", "Model:", "Serial:"
- `status_verbose_missing_disk` — "MISSING", "(not found)" or "device absent"
- `status_verbose_lsblk_failure` — "(unknown)" for model/serial, not fatal

**Error policy tests:**
- `status_scrub_completed` — timestamp appears
- `status_scrub_failure_tolerant` — command error → "unknown", not fatal
- `status_df_failure_fatal` — df error → `Err(StatusError::...)`
- `status_usage_failure_fatal` — usage error → `Err(StatusError::...)`
- `status_device_stats_failure_fatal` — stats error (when verbose) → `Err(StatusError::...)`
- `status_not_btrfs_maps_to_not_mounted` — `probe_pool` returns `NotBtrfs` → report has `status_code == NotMounted`, not an error

**Helpers:**
- `format_bytes_units` — B (0, 1, 1023), KiB (1024), MiB (1048576), GiB (1073741824), TiB (1099511627776)

### 5. NixOS VM test

**`tests/18-braid-status-rust.nix`** — follows `15-braid-plan-rust.nix` pattern:
- Takes `{ braid-rust }:` argument
- 3 virtual disks (disk1, disk2, disk3) at 1024 MiB
- Includes both bash `braid` (for setup) and `braid-rust` (for status testing)
- Config: 3 disks, mount `/mnt/storage`

**`tests/braid-status-rust.py`** — uses `braid-rust status`:

Phase 1 — Single-disk summary:
- `braid-rust status` — "healthy", "Drives:   1", "single", capacity lines, no "RAID1"/"missing"

Phase 2 — RAID1 healthy:
- `braid-rust status` — "healthy", "Drives:   3", "RAID1", capacity, "scrub", no "missing"
- `braid-rust status --verbose` — each disk "present" with "devid", "LUKS:", "Errors:"
- `braid-rust status --json --verbose` — parse JSON:
  - `status_code == "healthy"`
  - `len(disks) == 3`
  - each disk has `mapper`, `by_id`, `luks_uuid`, `devid`, `status == "present"`, `errors`
  - `errors` has `read`, `write`, `corruption` keys

Phase 3 — Degraded:
- `braid-rust status` — "DEGRADED", "missing", "RAID1", "2 present, 1 missing"
- `braid-rust status --verbose` — "MISSING", disk name, "not found" or "device absent"
- `braid-rust status --json --verbose` — parse JSON:
  - `status_code == "degraded"`
  - present disks have `status == "present"`
  - missing disk has `status == "missing"`

Phase 4 — Not mounted:
- `braid-rust status` — "not mounted"
- `braid-rust status --json` — parse JSON:
  - `schema_version == 1`
  - `status_code == "not_mounted"`
  - `status == "not mounted"`
  - `disks == []`
  - no `capacity`, `profile`, `total_devices` keys

**`flake.nix`** — add after `braid-apply-rust`:
```nix
braid-status-rust = pkgs.testers.nixosTest (import ./tests/18-braid-status-rust.nix {
  braid-rust = linuxCrane.braid-rust;
});
```

---

## Reused infrastructure

| What | Where |
|------|-------|
| `probe_pool()` | `cli/src/probe.rs:87` |
| `probe_config_disk()` | `cli/src/probe.rs:44` |
| `Filesystem` trait + `RealFilesystem` | `cli/src/probe.rs:12-21` |
| `mapper_name_for_by_id()` | `cli/src/plan.rs:465` (pub(crate)) |
| `parse_btrfs_df_json()` | `cli/src/parse/btrfs_filesystem_df.rs` |
| `parse_btrfs_filesystem_usage()` | `cli/src/parse/btrfs_filesystem_usage.rs` |
| `parse_btrfs_scrub_status()` | `cli/src/parse/btrfs_scrub_status.rs` |
| `parse_btrfs_device_stats()` | `cli/src/parse/btrfs_device_stats.rs` |
| `parse_lsblk_field()` | `cli/src/parse/lsblk.rs` |
| `MockRunner` | `cli/src/cmd.rs:216` |
| `CmdRequest::*` | `cli/src/cmd.rs:18` (all variants exist) |
| `Config`, `config_read()` | `cli/src/config.rs` |
| `PoolState`, `PoolDevice`, `ConfigDisk`, `ConfigDiskState` | `cli/src/types.rs` |
| `ScrubState`, `BtrfsDfOutput`, etc. | `cli/src/parse/types.rs` |

## Files modified

| File | Change |
|------|--------|
| `cli/src/status.rs` | **New** — types, error, cmd_status, gather, format, helpers, all unit tests |
| `cli/src/lib.rs` | Add `pub mod status;` |
| `cli/src/main.rs` | Wire `Commands::Status` → `cmd_status(runner, fs, config, verbose, json)` |
| `tests/18-braid-status-rust.nix` | **New** — VM test config |
| `tests/braid-status-rust.py` | **New** — VM test assertions with JSON verbose coverage |
| `flake.nix` | Register `braid-status-rust` check |

## Acceptance criteria

1. `cargo test -p braid-cli` — all unit tests pass (existing + new status tests)
2. `cargo test -p braid-cli --test golden_nixos_25_11` — golden tests still pass
3. `make test-one t=braid-status-rust` — VM integration test passes
4. `make test` — full suite still passes
