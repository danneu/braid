# Phase 2: Parse Layer — Typed Output Structs + Parsers

## Context

Phase 1 scaffold is done. `cmd.rs` has placeholder types using `serde_json::Value` and a `CmdOutput` enum that forces a dispatcher pattern. `parse.rs` is a stub. Phase 2 replaces all of this with concrete typed structs and individual parse functions — the foundation for probe/identity/plan/exec modules.

The hard rule from 5-plan.md: **parse.rs owns ALL parsing. Domain modules only see typed structs.**

## Files to modify

- `cli/Cargo.toml` — add `regex = "1"`
- `cli/src/cmd.rs` — remove `CmdOutput` enum + 3 placeholder structs, add 4 new `CmdRequest` variants
- `cli/src/parse/mod.rs` — re-exports, `ParseError`
- `cli/src/parse/types.rs` — all 10 typed output structs
- `cli/src/parse/json.rs` — 3 JSON parsers + tests
- `cli/src/parse/text.rs` — 7 text parsers + tests
- `cli/src/lib.rs` — `pub mod parse` stays (now a directory module)

## Steps

### 1. Add `regex` dependency to `cli/Cargo.toml`

### 2. Clean up `cmd.rs`

**Remove:**
- `LsblkJson`, `FindmntJson`, `BtrfsDfJson` structs (placeholder `serde_json::Value`)
- `CmdOutput` enum (entire thing — domain code calls parse functions directly)

**Add `LsblkFieldKind` enum and new `CmdRequest` variants:**
```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LsblkFieldKind {
    Model,
    Serial,
}

// New CmdRequest variants:
BtrfsFilesystemUsageRaw { mount_point: String },
BtrfsScrubStatus { mount_point: String },
BtrfsDeviceStats { mount_point: String },
LsblkField { device: String, field: LsblkFieldKind },
```

**Update** the `cmd_request_declares_expected_commands` test to cover all 10 variants.

### 3. Split `parse.rs` into `parse/` module directory

```
cli/src/parse/
  mod.rs    — re-exports, ParseError enum
  types.rs  — all typed output structs (stable contract)
  json.rs   — 3 JSON parsers + inline tests
  text.rs   — 7 text parsers + inline tests
```

### 4. `parse/types.rs` — typed output structs

Each struct contains **only fields domain code needs** (traced from `scripts/braid.sh`).

**JSON command structs:**

```rust
// lsblk --json --bytes
pub struct LsblkDevice {
    pub name: String,
    pub device_type: String,       // "disk", "part", "crypt"
    pub size: Option<u64>,         // bytes
    pub model: Option<String>,
    pub serial: Option<String>,
    pub uuid: Option<String>,      // filesystem UUID
    pub children: Vec<LsblkDevice>,
}
pub struct LsblkOutput { pub blockdevices: Vec<LsblkDevice> }

// findmnt --json
pub struct FindmntEntry { pub target: String, pub source: String, pub fstype: String }
pub struct FindmntOutput { pub filesystems: Vec<FindmntEntry> }

// btrfs --format json filesystem df
pub struct BtrfsDfEntry { pub bg_type: String, pub bg_profile: String, pub bg_used: u64, pub bg_total: u64 }
pub struct BtrfsDfOutput { pub entries: Vec<BtrfsDfEntry> }
```

**Text command structs:**

```rust
// btrfs filesystem show
pub struct BtrfsShowDevice { pub devid: u64, pub size: String, pub path: String }
pub struct BtrfsFilesystemShowOutput { pub total_devices: u64, pub devices: Vec<BtrfsShowDevice>, pub has_missing: bool }

// cryptsetup status
pub struct CryptsetupStatusOutput { pub is_active: bool, pub device: Option<String> }

// cryptsetup luksUUID
pub struct CryptsetupLuksUuidOutput { pub uuid: LuksUuid }  // uses uuid::Uuid::parse_str for validation

// btrfs filesystem usage --raw
pub struct BtrfsFilesystemUsageOutput { pub device_size_bytes: u64, pub used_bytes: u64, pub free_estimated_bytes: u64 }

// btrfs scrub status
pub enum ScrubState { Never, Completed { started_at: String }, Unknown }
pub struct BtrfsScrubStatusOutput { pub state: ScrubState }

// btrfs device stats
pub struct DeviceErrorStats { pub device_path: String, pub read_io_errs: u64, pub write_io_errs: u64, pub corruption_errs: u64, pub generation_errs: u64, pub flush_io_errs: u64 }
pub struct BtrfsDeviceStatsOutput { pub devices: Vec<DeviceErrorStats> }

// lsblk -ndo FIELD
pub struct LsblkFieldOutput { pub value: Option<String> }
```

### 5. `parse/mod.rs` — ParseError + re-exports

```rust
enum ParseError {
    InvalidJson { cmd: String, detail: String },
    InvalidText { cmd: String, detail: String },
    CommandFailed { cmd: String, exit_code: i32, stderr: String },
    MissingField { cmd: String, field: String },
}
```

Drop the `Unsupported` variant — after Phase 2 every parse path is implemented.

Re-export all types and parse functions from submodules.

### 6. Implement 10 parse functions

Each takes `&RawCommandOutput`, returns `Result<TypedStruct, ParseError>`.

**Exit-code handling policy:**
- **Default (most commands):** non-zero exit → `CommandFailed`
- **`cryptsetup status`:** non-zero exit → `is_active: false` ONLY if stderr is empty or contains expected patterns (e.g., "is not active"). Unexpected stderr → `CommandFailed`
- **`findmnt`:** non-zero exit → `FindmntOutput { filesystems: vec![] }` ONLY if stderr matches "not found" / empty. Unexpected stderr → `CommandFailed`

**`parse/json.rs`** (implementation order):
1. `parse_findmnt_json` — serde deserialize; benign non-zero exit if stderr matches expected pattern
2. `parse_lsblk_json` — serde deserialize with recursive children
3. `parse_btrfs_df_json` — serde deserialize, handle `"filesystem-df"` hyphenated top-level key. Include fixture test that validates the JSON shape matches what `btrfs --format json filesystem df` produces on the pinned nixpkgs toolchain

**`parse/text.rs`** (implementation order):
1. `parse_lsblk_field` — trim stdout, None if empty
2. `parse_cryptsetup_luks_uuid` — trim, validate with `uuid::Uuid::parse_str()` (no regex)
3. `parse_cryptsetup_status` — exit code + stderr check; extract "device:" line value
4. `parse_btrfs_scrub_status` — "no stats available" → Never, else extract "scrub started" datetime
5. `parse_btrfs_filesystem_usage` — extract "Device size:", "Used:", "Free (estimated):" trailing integers
6. `parse_btrfs_device_stats` — regex: group `[/dev/mapper/X].field_name  N` lines by device
7. `parse_btrfs_filesystem_show` — regex: "Total devices N", `devid\s+(\d+).*path\s+(.+)` lines, "missing" detection

### 7. Inline tests for every parser

Each parser gets at minimum:
- **Valid input test** — hand-crafted fixture matching real tool output
- **Malformed input test** — garbage/missing fields → `ParseError`
- **Exit-code edge case** — where applicable (cryptsetup status, findmnt, luksUUID)

Fixtures as `const &str` in `#[cfg(test)]` blocks, co-located with the parser.

| Parser | Valid fixtures | Malformed/edge fixtures |
|--------|---------------|------------------------|
| `parse_lsblk_json` | 2-disk tree with children | missing `blockdevices` key |
| `parse_findmnt_json` | mounted btrfs; not mounted (exit 1, expected stderr) | unexpected stderr on exit 1 |
| `parse_btrfs_df_json` | RAID1 profile; single profile | missing Data entry; wrong top-level key |
| `parse_btrfs_filesystem_show` | 3-disk healthy; 1-missing degraded | no "Total devices" line |
| `parse_cryptsetup_status` | active device; inactive (expected stderr); inactive (unexpected stderr → error) | active but no "device:" line |
| `parse_cryptsetup_luks_uuid` | valid UUID | non-UUID string; exit!=0 |
| `parse_btrfs_filesystem_usage` | normal usage output | missing "Device size:" |
| `parse_btrfs_scrub_status` | never scrubbed; has been scrubbed | empty output |
| `parse_btrfs_device_stats` | 2 devices zero errors; nonzero errors | empty |
| `parse_lsblk_field` | model string; empty | (too simple) |

### 8. Capture `btrfs --format json filesystem df` fixture

Run in a VM (or hand-craft from btrfs-progs source for the pinned nixpkgs version) to nail down the exact JSON key names (`bg_type` vs `type`, `filesystem-df` wrapper key, etc.). Use this as the fixture in `parse_btrfs_df_json` tests. This validates the JSON shape in Phase 2, not Phase 3.5.

### 9. Regenerate Cargo.lock and verify

```bash
cd cli && cargo generate-lockfile && cargo test
```

All Phase 1 tests + new parser tests must pass.

## Key decisions

- **CmdOutput enum deleted** — parse functions return typed structs directly, no dispatcher
- **`parse/` is a directory module** — `types.rs` (stable contract), `json.rs`, `text.rs` keep concerns separated
- **`LsblkFieldKind` enum** — compile-time safety, not `field: String`
- **`uuid::Uuid::parse_str`** for UUID validation — no regex
- **Non-zero exit ≠ automatic benign** — only treat as benign when stderr matches expected patterns; unexpected stderr → `CommandFailed`
- **btrfs df JSON shape validated in Phase 2** fixture tests, not deferred to Phase 3.5
- **regex added** — for btrfs show/stats text parsing
