# Phase 3.5: Probe + CLI Wiring + VM Planner Validation

## Context

Phase 3 (pure planner logic) is complete: `compute_plan()` takes typed inputs and returns `PlanOutcome`, with 23 unit tests. But `braid plan` in main.rs prints "not yet implemented". This phase bridges the gap: implement the I/O probe layer, wire the `plan` subcommand end-to-end, and validate it in a NixOS VM test against real virtual disks.

## Files to modify/create

- `cli/src/plan.rs` — make `mapper_name_for_by_id` + `BY_ID_PREFIX` `pub(crate)`, update degraded warning message
- `cli/src/cmd.rs` — implement `RealRunner` (actual `std::process::Command` dispatch)
- `cli/src/probe.rs` — **NEW** — `Filesystem` trait + `probe_config_disk()` + `probe_pool()` + unit tests
- `cli/src/lib.rs` — add `pub mod probe`
- `cli/src/main.rs` — wire `plan` subcommand: config → probe → compute_plan → output
- `tests/15-braid-plan-rust.nix` — **NEW** — NixOS VM test config
- `tests/braid-plan-rust.py` — **NEW** — VM test script (13 subtests)
- `flake.nix` — register `braid-plan-rust` in checksFor

## Steps

### 1. Visibility changes in plan.rs

Make two items `pub(crate)` so probe.rs can reuse them:
- `BY_ID_PREFIX` (line 463)
- `mapper_name_for_by_id` (line 465)

Update the degraded pool warning message to include actionable guidance:
```
"pool is degraded: {N} missing device(s). To evict, run: braid apply --allow-remove-missing"
```
This matches the bash test expectation (braid-plan.py line 224).

### 2. Implement RealRunner in cmd.rs

Replace the stub with real `std::process::Command` dispatch. Maps each `CmdRequest` variant to a command with fully explicit flags:

| CmdRequest | Command |
|---|---|
| `LsblkJson` | `lsblk --json --bytes --output NAME,TYPE,SIZE,MODEL,SERIAL,UUID` |
| `FindmntJson` | `findmnt --json --output TARGET,SOURCE,FSTYPE -T {mount_point}` |
| `BtrfsFilesystemShow` | `btrfs filesystem show {mount_point}` |
| `CryptsetupStatus` | `cryptsetup status {mapper}` |
| `CryptsetupLuksUuid` | `cryptsetup luksUUID {device}` |
| `BtrfsFilesystemDfJson` | `btrfs --format json filesystem df {mount_point}` |
| `BtrfsFilesystemUsageRaw` | `btrfs filesystem usage --raw {mount_point}` |
| `BtrfsScrubStatus` | `btrfs scrub status {mount_point}` |
| `BtrfsDeviceStats` | `btrfs device stats {mount_point}` |
| `LsblkField` | `lsblk -ndo {FIELD} {device}` |

Pin output columns for `lsblk` (`--output NAME,TYPE,SIZE,MODEL,SERIAL,UUID`) and `findmnt` (`--output TARGET,SOURCE,FSTYPE`) so output doesn't change if system defaults or terminal width change. Other commands (`cryptsetup`, `btrfs`) have stable default output.

**Critical:** `RealRunner` returns `Ok(RawCommandOutput)` even for non-zero exit codes. `Err(CmdError::Failed)` only for process-level failures (binary not found, signal death). Parsers inspect exit codes for semantic meaning (not-LUKS, not-mounted).

### 3. Create probe.rs

**`Filesystem` trait** — abstracts `Path::exists()` for testability:
```rust
pub trait Filesystem {
    fn exists(&self, path: &str) -> bool;
}
pub struct RealFilesystem;
// impl: delegates to std::path::Path::new(path).exists()
```

**`ProbeError` enum:**
```rust
#[derive(Debug, thiserror::Error)]
pub enum ProbeError {
    #[error("command failed: {0}")]
    Cmd(#[from] crate::cmd::CmdError),
    #[error("parse error: {0}")]
    Parse(#[from] crate::parse::ParseError),
    #[error("pool device {mapper}: {detail}")]
    PoolDevice { mapper: String, detail: String },
    #[error("{mount_point} is mounted but fstype is {fstype}, not btrfs")]
    NotBtrfs { mount_point: String, fstype: String },
}
```

**`probe_config_disk(runner, fs, by_id_path) → Result<ConfigDisk, ProbeError>`:**
1. `fs.exists(by_id_path)` → false → `ConfigDiskState::Absent`
2. `runner.run(CryptsetupLuksUuid { device })`:
   - `Err(CmdError::Failed)` → **propagate as ProbeError** (process spawn failure, binary missing — real I/O error)
   - `Ok(raw)` with `parse` returning `ParseError::CommandFailed` (non-zero exit) → **benign: `PresentNotLuks`**
   - `Ok(raw)` with `parse` returning `ParseError::InvalidText`/`InvalidJson` → **propagate as ProbeError** (unexpected output from a successful command is not benign)
   - `Ok(raw)` with `parse` returning `Ok(uuid)` → have UUID, continue
3. `mapper_name_for_by_id(by_id_path)` → derive mapper name, `fs.exists("/dev/mapper/{name}")` → `mapper_open`
4. Return `PresentLuks { uuid, mapper_open }`

**Non-LUKS detection policy:** Only `ParseError::CommandFailed` (non-zero exit code) is treated as "not LUKS". All other errors (spawn failure, garbled output) propagate. This prevents masking real failures as benign PresentNotLuks.

**`probe_pool(runner, mount_point) → Result<PoolState, ProbeError>`:**
1. `FindmntJson` → if empty filesystems → `PoolState { mounted: false, ... }`
2. If mounted, **validate fstype is "btrfs"** → if not, return `Err(ProbeError::NotBtrfs { mount_point, fstype })`. Hard fail.
3. `BtrfsFilesystemShow` → get device list + total_devices + has_missing
4. For each btrfs device path:
   - **Guard:** require path starts with `/dev/mapper/`. If not → `Err(ProbeError::PoolDevice { mapper: path, detail: "not a /dev/mapper/ path" })`. Hard fail — braid only manages LUKS-on-btrfs pools; a non-mapper device in the pool is an assumption violation.
   - Extract `name` from `/dev/mapper/{name}`.
   - `CryptsetupStatus { mapper: name }` → **must be active with a device field**. If `is_active: false` or `device: None` → `Err(ProbeError::PoolDevice { mapper, detail: "not active" })`. Hard fail — a mounted btrfs pool device that isn't an active LUKS mapper is an inconsistency we can't reason about.
   - `CryptsetupLuksUuid { device: underlying }` → get UUID. Failure here is a hard error (pool device should always be LUKS).
   - Build `PoolDevice { mapper, luks_uuid, devid }`
5. `missing_count = total_devices.saturating_sub(devices.len() as u64)` — defensive against parser inconsistency where `devices.len() > total_devices`

**No new CmdRequest variants needed.** `CryptsetupLuksUuid` failure serves as the "not LUKS" signal (probe_config_disk only; probe_pool treats it as hard error).

**Unit tests (~13):**

| Test | Scenario |
|---|---|
| `probe_config_disk_absent` | Device doesn't exist → `Absent` |
| `probe_config_disk_present_not_luks` | Exists, luksUUID exits non-zero → `PresentNotLuks` |
| `probe_config_disk_cmd_spawn_fails` | `runner.run()` returns `CmdError::Failed` → propagates as `ProbeError::Cmd` (not silent PresentNotLuks) |
| `probe_config_disk_garbled_uuid_output` | luksUUID exits 0 but output is garbage → propagates as `ProbeError::Parse` (not silent PresentNotLuks) |
| `probe_config_disk_present_luks_closed` | LUKS, mapper not open → `PresentLuks { mapper_open: false }` |
| `probe_config_disk_present_luks_open` | LUKS, mapper open → `PresentLuks { mapper_open: true }` |
| `probe_pool_unmounted` | findmnt empty → `mounted: false` |
| `probe_pool_mounted_not_btrfs` | findmnt returns ext4 → `ProbeError::NotBtrfs` |
| `probe_pool_mounted_2disk` | 2 devices, trace LUKS UUIDs, correct mapper/uuid/devid |
| `probe_pool_mounted_with_missing` | total_devices=3, 2 present → `missing_count: 1` (saturating_sub) |
| `probe_pool_mapper_not_active` | cryptsetup status returns inactive for pool device → `ProbeError::PoolDevice` |
| `probe_pool_non_mapper_device` | btrfs show returns `/dev/sda1` (not `/dev/mapper/...`) → `ProbeError::PoolDevice` |
| `probe_pool_missing_count_saturates` | total_devices=0, 1 present device (parser bug) → `missing_count: 0` (not underflow) |

### 4. Wire plan subcommand in main.rs

```rust
Commands::Plan(args) => {
    let config = config_read(Path::new(&config_path))?;
    let runner = RealRunner;
    let fs = RealFilesystem;
    let config_disks = config.disks.iter()
        .map(|d| probe_config_disk(&runner, &fs, d))
        .collect::<Result<Vec<_>, _>>()?;
    let pool = probe_pool(&runner, &config.mount_point)?;
    let flags = PlanFlags { allow_remove_missing, allow_remove_ambiguous };
    let outcome = compute_plan(&config, &config_disks, &pool, &flags);
    let report = to_plan_report(&outcome, &config);
    // --json → serde_json::to_string_pretty, else → format_plan_human
}
```

**Exit code contract:**
- **Exit 0** — plan produced successfully (applicable, applicable_with_warnings, OR blocked). All three are valid plan results.
- **Exit 1** — runtime error: config not found, config parse failure, probe failure (command not found, I/O error, unexpected parse error). These mean "couldn't compute a plan at all."

Use `std::process::exit(1)` in the error paths. The plan subcommand never exits non-zero for a blocked plan — "blocked" is a valid answer, not an error.

Update `lib.rs` to add `pub mod probe;`.

### 5. NixOS VM test

**`tests/15-braid-plan-rust.nix`** — 4 virtual disks. Both binaries in VM:
- Bash `braid` (for setup: init-disk, apply)
- Rust `braid-rust` (for plan validation, renamed via `postInstall = "mv $out/bin/braid $out/bin/braid-rust"`)

Rust binary built inside test .nix via `pkgs.rustPlatform.buildRustPackage { src = ../cli; ... }`.

**`tests/braid-plan-rust.py`** — 13 subtests:

| # | Subtest | Validates |
|---|---------|-----------|
| 0 | Setup: build 2-disk RAID1 pool | (bash init-disk + apply) |
| 1 | No-op plan | `status == "applicable"`, 0 mutation actions |
| 2 | Non-LUKS disk warning | `INIT_REQUIRED` in warning codes |
| 3 | After init-disk → OPEN_LUKS + ADD | action types + target contains disk name |
| 4 | Absent disk → DISK_ABSENT_SKIPPED | warning code + disk path in message |
| 5 | Absent blocks removal | `status == "blocked"`, `IDENTITY_AMBIGUOUS_ABSENT_DISK` |
| 6 | --allow-remove-ambiguous | 2 confirmations (ambiguity + redundancy) |
| 7 | Graceful remove | `REMOVE_DISK_GRACEFUL` + `CLOSE_LUKS_MAPPER`, correct target |
| 8 | Redundancy confirmation | phrase contains "redundancy" |
| 9 | Degraded pool warning text | `POOL_DEGRADED_MISSING_DEVICES` warning contains `--allow-remove-missing` hint |
| 10 | JSON schema validation | All fields present, types correct, summary consistency |
| 11 | Human output format | Plan ID, Mount, Status, action lines |
| 12 | Bootstrap (unmounted) | OPEN_LUKS + ADD, no BALANCE/REMOVE |

Subtest 9 (degraded warning) requires setup: kill one disk, mount degraded. Validates that the POOL_DEGRADED warning message includes `--allow-remove-missing` (the actionable hint added in step 1).

Test helpers: `rust_plan(extra)` and `rust_plan_json()` call `braid-rust plan --config ...`.

### 6. Register test in flake.nix

Add to `checksFor`:
```nix
braid-plan-rust = pkgs.testers.nixosTest (import ./tests/15-braid-plan-rust.nix);
```

## JSON contract differences from bash

The VM test assertions account for these structural differences:

1. **Warnings:** Rust serializes as `{"code": "INIT_REQUIRED", "message": "..."}` (object). Bash uses `"INIT_REQUIRED: ..."` (string). Test uses `w["code"]`.
2. **Status:** Rust has `"applicable_with_warnings"` when warnings exist. Bash always uses `"applicable"`. Test checks `status in ["applicable", "applicable_with_warnings"]`.
3. **No-op verify:** Rust produces 0 actions for no-op. Bash always appends VERIFY_*. Test asserts `len(mutation_actions) == 0`.

## Key decisions

- **No new CmdRequest variants** — `CryptsetupLuksUuid` failure = not LUKS. Simple.
- **Filesystem trait for testability** — `Filesystem::exists()` wraps `Path::exists()`. MockFilesystem for probe unit tests.
- **`mapper_name_for_by_id` shared via pub(crate)** — probe.rs reuses planner's by-id basename derivation + validation.
- **Rust binary renamed to `braid-rust` in VM** — avoids PATH collision with bash `braid` needed for setup.
- **RealRunner always returns Ok for non-zero exit** — parsers own exit-code semantics, not the runner.

## Verification

```bash
cd cli && cargo test                        # All unit tests (Phases 1-3 + probe)
make test-one t=braid-plan-rust             # New VM test
make test-one t=braid-plan                  # Existing bash test (no regression)
```
