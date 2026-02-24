# Plan: btrfs Progress-Monitoring Parsers

## Context

The three long-running btrfs operations (balance to raid1, balance to single, device remove) need progress monitoring for a future TUI/daemon. Each is pollable via a separate btrfs command:

- **Balance** (both raid1 and single conversions): poll `btrfs balance status <mount>`
- **Device remove**: poll `btrfs device usage --raw <mount>` — watch target device's used bytes decrease toward 0

Neither command has JSON output — both are plain text. This plan adds the parsing layer, CmdRequest variants, fixture capture, and tests. No orchestration/spawn logic yet — just the core functions the daemon will call.

## Implementation

### 1. Output types — `cli/src/parse/types.rs`

```rust
/// btrfs balance status
pub enum BalanceState {
    None,                            // "No balance found on '/mnt/storage'"
    Running { done_chunks: u64, estimated_total_chunks: u64, considered_chunks: u64, pct_left: u8 },
    Paused  { done_chunks: u64, estimated_total_chunks: u64, considered_chunks: u64, pct_left: u8 },
}
pub struct BtrfsBalanceStatusOutput { pub state: BalanceState }

/// btrfs device usage --raw
pub struct DeviceAllocation { pub alloc_type: String, pub profile: String, pub bytes: u64 }
pub struct BtrfsDeviceUsageEntry {
    pub path: String, pub devid: u64, pub device_size: u64, pub device_slack: u64,
    pub allocations: Vec<DeviceAllocation>, pub unallocated: u64,
}
impl BtrfsDeviceUsageEntry {
    pub fn used_bytes(&self) -> u64 { self.allocations.iter().map(|a| a.bytes).sum() }
}
pub struct BtrfsDeviceUsageOutput { pub devices: Vec<BtrfsDeviceUsageEntry> }
```

### 2. CmdRequest variants — `cli/src/cmd.rs`

Add two read-only polling commands:
- `BtrfsBalanceStatus { mount_point }` → `btrfs balance status <mount>`
- `BtrfsDeviceUsageRaw { mount_point }` → `btrfs device usage --raw <mount>`

Add `RealRunner::run()` match arms. Update `cmd_request_declares_expected_commands` test (24 → 26).

### 3. Balance status parser — **new** `cli/src/parse/btrfs_balance_status.rs`

**Style:** Simple text extraction (like `btrfs_scrub_status.rs`), but **pattern-based** — match semantic line patterns anywhere in stdout rather than relying on positional first/second lines.

Parse logic:
1. Exit status != 0 → `ParseError::CommandFailed`
2. Scan all lines for semantic patterns (order-independent):
   - Any line containing `"No balance found"` → `BalanceState::None`
   - Any line containing `"is running"` → state = Running
   - Any line containing `"is paused"` → state = Paused
   - Any line matching `N out of about M chunks balanced (K considered), P% left` → extract the four integers via string splitting
3. Combine: state variant + chunk progress fields
4. If no recognizable pattern found → `ParseError::InvalidText`

This keeps the parser stable against extra lines, reordering, or btrfs-progs formatting changes while staying lightweight (no full grammar needed).

**Tests (inline in module):**
- Contract: `btrfs-balance-status-none.txt` fixture
- Synthetic: running output (inline string)
- Synthetic: paused output (inline string)
- Synthetic: running with extra diagnostic lines interspersed (robustness)
- Synthetic: error/empty cases

### 4. Device usage parser — **new** `cli/src/parse/btrfs_device_usage.rs`

**Style:** nom combinators (like `btrfs_device_stats.rs` — repeated per-device blocks with structured header + key-value lines).

**Compatibility policy:** Forward-compatible. Require core fields (`Device size`, `Device slack`, `Unallocated`, device header). Collect known allocation lines (`Data,*`, `Metadata,*`, `System,*`, `GlobalReserve,*`). **Ignore unknown keys** silently — don't fail on new fields added in future btrfs-progs versions. Include a test proving unknown keys are ignored.

Parse logic:
1. Exit status != 0 → `ParseError::CommandFailed`
2. nom header parser: `<path>, ID: <u64>`
3. nom key-value parser: `<key>: <u64>` (indented lines)
4. Keys containing `,` (like `"Data,RAID1"`) split on `,` → `alloc_type` + `profile` → stored in `allocations`
5. Required keys: `"Device size"`, `"Device slack"`, `"Unallocated"` → dedicated fields; `MissingField` error if absent
6. Any other key → silently ignored

**Tests (inline in module):**
- Contract: `btrfs-device-usage-2disk.txt` fixture
- Synthetic: single device, various profiles
- Synthetic: unknown keys are silently ignored (forward-compat proof)
- Synthetic: `used_bytes()` helper correctness
- Synthetic: error/malformed/missing required field cases

### 5. Document compatibility exception — `cli/docs/command-capabilities.md`

Add a row to the Exceptions table:

| `parse_btrfs_device_usage` | Unknown allocation keys are silently ignored | btrfs-progs may add new per-device allocation categories in future versions. Required fields (`Device size`, `Device slack`, `Unallocated`, device header) are fail-hard. Allocation lines (comma-separated type,profile keys) are collected; unrecognized indented key-value lines are dropped. Domain code only sees the typed `BtrfsDeviceUsageEntry` struct. |

### 6. Register modules — `cli/src/parse/mod.rs`

Add `pub mod btrfs_balance_status;` and `pub mod btrfs_device_usage;` + re-exports.

### 7. Fixture capture — `tests/capture-tool-fixtures.py`

Add before the `umount` line (line 99):
```python
# 12. btrfs balance status (idle — no balance running)
machine.succeed(f"btrfs balance status {MOUNT} > {FIXTURE_DIR}/btrfs-balance-status-none.txt")

# 13. btrfs device usage --raw (per-device allocation breakdown)
machine.succeed(f"btrfs device usage --raw {MOUNT} > {FIXTURE_DIR}/btrfs-device-usage-2disk.txt")
```

No changes needed to `capture-tool-fixtures.nix` — same VM, same packages.

**Running balance fixture:** Not captured from VM. On tiny disks (512MB), balance completes in <1s — capturing mid-balance is fragile. The "running" format is covered by synthetic inline tests.

### 8. Golden tests — `cli/tests/golden_nixos_25_11.rs`

Add two `golden_test!` entries with **strong contract assertions** (not just positivity checks):

- `golden_btrfs_balance_status_none` — asserts exact `BalanceState::None`
- `golden_btrfs_device_usage` — asserts:
  - Exactly 2 devices
  - Exact devid/path mapping (devid 1 → `braid-vdb`, devid 2 → `braid-vdc`)
  - At least one specific allocation tuple checked (e.g. first device has a `Data`/`RAID1` allocation with bytes > 0)
  - `device_size > 0` and `unallocated > 0` as secondary sanity checks

## Files Summary

| File | Action |
|------|--------|
| `cli/src/parse/types.rs` | Add 6 new types |
| `cli/src/cmd.rs` | Add 2 CmdRequest variants + RealRunner arms + update test count |
| `cli/src/parse/btrfs_balance_status.rs` | **Create** — simple text parser + tests |
| `cli/src/parse/btrfs_device_usage.rs` | **Create** — nom parser + tests |
| `cli/docs/command-capabilities.md` | Add exception row for `parse_btrfs_device_usage` |
| `cli/src/parse/mod.rs` | Add 2 module declarations + 2 re-exports |
| `tests/capture-tool-fixtures.py` | Add 2 capture commands |
| `cli/tests/golden_nixos_25_11.rs` | Add 2 golden test entries |

## Verification

1. `cd cli && cargo test` — all unit tests pass (synthetic tests validate immediately; golden tests skip gracefully until fixtures captured)
2. `make capture-fixtures` — generates `btrfs-balance-status-none.txt` and `btrfs-device-usage-2disk.txt`
3. `cd cli && cargo test` again — golden tests now pass against real VM output
4. `make test-rust` — same as step 3, via Makefile
