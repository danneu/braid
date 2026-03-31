# Plan: S.M.A.R.T Tab in Disk Detail Modal

## Context

The disk detail modal currently shows LUKS info and btrfs allocations but no SMART data. Users have to run `smartctl` manually to see drive health details. The TUI already runs `smartctl -H -A --json` per disk during probe but discards everything except a Healthy/Degraded/Failing/Unknown classification. This plan extends the parser to preserve SMART detail and adds a new tab to display it.

## Approach

### 1. Extend smartctl parser to extract detail

**File: `cli/src/parse/smartctl.rs`**

Add top-level fields to `RawSmartctlOutput`:
```rust
#[serde(default)]
temperature: Option<RawTemperature>,
#[serde(default)]
power_on_time: Option<RawPowerOnTime>,
#[serde(default)]
power_cycle_count: Option<u64>,
```

With helper structs:
```rust
#[derive(Deserialize)]
struct RawTemperature { #[serde(default)] current: Option<u64> }

#[derive(Deserialize)]
struct RawPowerOnTime { #[serde(default)] hours: Option<u64> }
```

Also extend `RawNvmeHealth` with `unsafe_shutdowns: u64`.

Add new public function `parse_smartctl_detail(raw: &RawCommandOutput) -> SmartDetail` that:
- Parses the same JSON as `parse_smartctl_health`
- Extracts summary (passed, temp, power hours, cycles) from top-level fields
- Detects protocol (NVMe vs SATA) same as existing logic
- For SATA: filters `ata_smart_attributes.table` to 5 curated attributes, each with a hardcoded `&'static str` description
- For NVMe: extracts fields from `nvme_smart_health_information_log`
- Returns `SmartDetail::Unknown` on parse failure

SATA attributes to include (matched by name string from `ata_smart_attributes.table[].name`):

| Name                    | Description (for TUI column)        |
|-------------------------|-------------------------------------|
| `End-to-End_Error`      | Data path integrity failure         |
| `Reported_Uncorrect`    | Uncorrectable read errors           |
| `Reallocated_Event_Count` | Sector remap operations           |
| `Spin_Retry_Count`      | Failed spin-up attempts             |
| `Command_Timeout`       | Incomplete drive commands           |

**File: `cli/src/parse/types.rs`** — add new types:

```rust
pub struct SmartSummary {
    pub passed: bool,
    pub temperature_celsius: Option<u64>,
    pub power_on_hours: Option<u64>,
    pub power_cycle_count: Option<u64>,
}

pub struct SataSmartAttribute {
    pub name: String,
    pub raw_value: u64,
    pub description: &'static str,
}

pub struct NvmeSmartDetail {
    pub critical_warning: u64,
    pub media_errors: u64,
    pub available_spare: u64,
    pub available_spare_threshold: u64,
    pub percentage_used: u64,
    pub unsafe_shutdowns: u64,
}

pub enum SmartDetail {
    Sata { summary: SmartSummary, attributes: Vec<SataSmartAttribute> },
    Nvme { summary: SmartSummary, detail: NvmeSmartDetail },
    Unknown,
}
```

**File: `cli/src/parse/mod.rs`** — re-export `parse_smartctl_detail`.

### 2. Store SMART detail in PoolState

**File: `cli/src/tui/model.rs`**

Add to `PoolState`:
```rust
pub smart_detail: HashMap<String, SmartDetail>,
```

### 3. Populate during probe

**File: `cli/src/tui/probe.rs`**

Refactor the per-disk smartctl loop to run the command once and parse both health and detail from the same output (avoid running smartctl twice):

```rust
let raw = runner.run(&CmdRequest::SmartctlHealthJson { device: by_id_path.clone() });
let health = raw.as_ref().map(|r| parse_smartctl_health(r)).unwrap_or(SmartHealth::Unknown);
let detail = raw.as_ref().map(|r| parse_smartctl_detail(r)).unwrap_or(SmartDetail::Unknown);
smart_health.insert(disk_name.clone(), health);
smart_detail.insert(disk_name.clone(), detail);
```

### 4. Add tab system to disk detail modal

**File: `cli/src/tui/model.rs`** — add `DiskDetailTab` enum:

```rust
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DiskDetailTab { Info, Smart }
```

With `label()` → `"Info"` / `"S.M.A.R.T"`, `next()`, `prev()`, and `ALL` const.

Add `disk_detail_tab: DiskDetailTab` to `Model`, default `Info`.

**File: `cli/src/tui/app.rs`** — add messages:

- `Message::NextDetailTab` → `model.disk_detail_tab = model.disk_detail_tab.next()`
- `Message::PrevDetailTab` → `model.disk_detail_tab = model.disk_detail_tab.prev()`
- In `Message::CloseDiskDetail` handler, reset `disk_detail_tab` to `Info`

**File: `cli/src/tui/keymap.rs`** — in the `show_disk_detail` branch, add:

```rust
KeyCode::Tab => Some(Message::NextDetailTab),
KeyCode::BackTab => Some(Message::PrevDetailTab),
```

### 5. Render the S.M.A.R.T tab

**File: `cli/src/tui/view/mod.rs`**

Refactor `view_disk_detail()`:

1. Add a `detail_tab_bar(active: DiskDetailTab)` function (same pattern as existing `tab_bar()`)
2. Tab bar renders at top of modal, below border
3. Based on `model.disk_detail_tab`, render either existing Info content or new SMART content
4. Increase modal width from `48` to `60`
5. Update footer to: `"Tab ⇆ · r reload · Esc back"` (or similar concise form)

**SMART tab layout for SATA:**
```
 Info   S.M.A.R.T

 Status     PASSED
 Temp       27 °C
 Power on   5,141 hours
 Cycles     397

 Key Attributes ──────────────────────────
 Name                   Value  Note
 End-to-End_Error           0  Data path integrity failure
 Reported_Uncorrect         0  Uncorrectable read errors
 Reallocated_Event_Ct       0  Sector remap operations
 Spin_Retry_Count           0  Failed spin-up attempts
 Command_Timeout            0  Incomplete drive commands

 Tab ⇆ · r reload · Esc back
```

**SMART tab layout for NVMe:**
```
 Info   S.M.A.R.T

 Status     PASSED
 Temp       36 °C
 Power on   2,736 hours
 Cycles     1,336

 NVMe Health ─────────────────────────────
 Critical Warning       0
 Media Errors           0
 Available Spare        100%  (threshold: 10%)
 Percentage Used        1%
 Unsafe Shutdowns       68

 Tab ⇆ · r reload · Esc back
```

**`SmartDetail::Unknown`:** render `"S.M.A.R.T data unavailable"` in dim.

Non-zero values in the SATA attributes table should be highlighted (Yellow or Red depending on severity — any non-zero for these curated attributes is concerning, so use Yellow).

### 6. Tests

**Parser tests** (`cli/src/parse/smartctl.rs`):
- Existing NVMe fixture → verify `parse_smartctl_detail` returns correct `SmartDetail::Nvme` with expected summary and detail fields
- Synthetic SATA JSON → verify only curated attributes are included (filters out Raw_Read_Error_Rate etc.)
- Bad JSON → returns `SmartDetail::Unknown`

**Keymap tests** (`cli/src/tui/keymap.rs`):
- Tab in disk detail → `NextDetailTab`
- BackTab in disk detail → `PrevDetailTab`

**Snapshot tests** (`cli/src/tui/view/mod.rs`):
- Update `sample_pool()` to include `smart_detail` field
- Add `snapshot_disk_detail_smart_sata` — SMART tab with SATA data
- Add `snapshot_disk_detail_smart_nvme` — SMART tab with NVMe data
- Add `snapshot_disk_detail_smart_unknown` — SMART tab with no data

## Files to modify

| File | Change |
|------|--------|
| `cli/src/parse/types.rs` | Add `SmartSummary`, `SataSmartAttribute`, `NvmeSmartDetail`, `SmartDetail` |
| `cli/src/parse/smartctl.rs` | Extend deserialization structs, add `parse_smartctl_detail()`, tests |
| `cli/src/parse/mod.rs` | Re-export `parse_smartctl_detail` |
| `cli/src/tui/model.rs` | Add `DiskDetailTab`, `disk_detail_tab` field, `smart_detail` in PoolState |
| `cli/src/tui/app.rs` | Add `NextDetailTab`/`PrevDetailTab` messages + handlers |
| `cli/src/tui/keymap.rs` | Route Tab/BackTab in disk detail mode |
| `cli/src/tui/probe.rs` | Parse SMART detail from existing command output |
| `cli/src/tui/view/mod.rs` | Tab bar in modal, SMART tab rendering, width increase |

## Verification

1. `just test-rust` — all unit and snapshot tests pass
2. Manual: open TUI, select a disk, press Enter, verify Info tab shows existing content
3. Manual: press Tab to switch to S.M.A.R.T tab, verify SMART data renders correctly
4. Manual: press Tab/BackTab to toggle between tabs
5. Manual: press Esc, reopen — should start on Info tab
6. Manual: test with both SATA and NVMe drives if available
