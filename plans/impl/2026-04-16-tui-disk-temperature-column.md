# Per-disk temperature column in the TUI

## Context

braid's TUI disks table (see `cli/src/tui/view/snapshots/snapshot_with_pool.snap`) currently shows Name, Bus, SMART, btrfs errors, and allocated. Users testing fan setups during a large copy have no way to see disk temperature, nor how hot a drive got during the session.

We already fetch `smartctl -H -A <dev> --json` on every TUI probe tick and parse it via `parse_smartctl_health` (`cli/src/parse/smartctl.rs:68`). The JSON contains `temperature.current` (populated from SMART attribute 194; confirmed in vendored `reference/smartmontools/smartmontools/ataprint.cpp:1379`), but we don't extract it.

**What we're adding:** A `Temp` column rendering `38° 32/45` — current plus session-scoped low/high watermarks. Watermarks live in TUI state (not in the probe), and capital `R` resets them globally. Drive-lifetime min/max (SCT status / device statistics log) is explicitly **not** what we want — those are persistent across power cycles and don't answer "how hot did this drive get during today's copy?"

## Design

- **Probe invocation unchanged.** Just extract `temperature.current` from the existing JSON.
- **Split the smartctl parser.** Return a struct carrying both `SmartHealth` and `Option<i16>` temperature (Celsius) rather than only health. Name it `SmartProbe { health: SmartHealth, celsius: Option<i16> }`. The `None` case covers: USB drives with no SMART passthrough, parser failure, and drives that just don't emit `temperature.current`.
- **Physical-identity keying for watermarks.** Config disk names (e.g. `"toshiba"`) are stable across ticks but are slot-shaped — swapping a different physical drive into the same slot would inherit the old drive's hi/lo. To avoid that, define:
  ```rust
  pub enum TemperatureDiskId {
      LuksUuid(crate::types::LuksUuid),    // preferred — from the pool probe's devices list
      ByIdPath(crate::types::ByIdPath),    // fallback — from Model.disk_by_id
  }
  ```
  Reuses the existing newtypes from `cli/src/types.rs:4,10` so UUIDs and paths can't get mixed up at call sites.
  LUKS UUID is available from `domain.devices` on the probe (the same list used to build `devid_to_name` at `cli/src/tui/probe.rs:48-52`). Do **not** add UUID to `DiskLuksInfo` (that struct represents `cryptsetup luksDump` detail, not identity). Instead, mirror the `devid_to_name` pattern to build a `disk_name -> LuksUuid` map from `domain.devices` and consult it when constructing each `TemperatureReading`. If a disk's UUID isn't present (shouldn't normally happen for pool members), fall back to the `/dev/disk/by-id/...` path stored in `Model.disk_by_id`.
- **Per-tick readings carry both name and physical id.** Add to `PoolState`:
  ```rust
  pub disk_temperature_readings: HashMap<String, TemperatureReading>,
  // where: pub struct TemperatureReading { pub id: TemperatureDiskId, pub celsius: i16 }
  // (celsius is a signed i16; SMART can report negative temps on cold-storage drives.)
  ```
  Keyed by config name so the view can render by the row it's already iterating, but each entry carries the physical id so the app can look up the right watermark. Absence of a key for a disk means "no current reading this tick" → cell renders `-`.
- **Watermarks live on `Model`, keyed by physical id.** `PoolState` is replaced wholesale each probe; watermarks must survive that. Add to `Model`:
  ```rust
  pub session_temperature_stats: HashMap<TemperatureDiskId, TemperatureWatermark>,
  // where: pub struct TemperatureWatermark { min_celsius: i16, max_celsius: i16, sample_count: u32 }
  ```
  No `last` field — the current reading is authoritative from the latest `PoolState`. This prevents showing stale temps if a probe fails or a drive drops off.
- **Update hook.** In the `Message::PoolProbeFinished` arm (`cli/src/tui/app.rs:123-140`), update `model.session_temperature_stats` from the owned `pool` first (iterate `pool.disk_temperature_readings.values()` and fold by `reading.id`: seed `{min_celsius = max_celsius = reading.celsius, sample_count = 1}` on first sample; on later samples widen `min_celsius`/`max_celsius` and bump `sample_count`), *then* move `pool` into `model.pool`. Doing it in this order avoids a borrow-check juggle.
- **Reset.** Capital `R` → `Message::ResetTemperatureStats` → clears the whole `session_temperature_stats` map. Silent — no toast.
- **Reset works in the disk-detail overlay too, but not while help is open.** Place the handler *after* the `show_help` branch (which closes help on any key) and *before* the `show_disk_detail` branch. This preserves the existing "any key closes help" behavior — `R` isn't advertised in the help overlay, and silently mutating stats while help is visible would be surprising.
- **Render rule** (new cell in `disk_table()` at `cli/src/tui/view/mod.rs:357-485`):
  - No reading for the disk this tick → `-` (entire cell).
  - Reading present, look up watermark by `reading.id`:
    - `sample_count < 2` → `38° --/--`.
    - `sample_count >= 2` → `38° 32/45`.
- **Footer** (`cli/src/tui/view/mod.rs:913-930`): append `Reset temp hi/lo: R` segment to the dynamic footer string.

## Implementation steps

### 1. Parser: extract temperature.current

File: `cli/src/parse/smartctl.rs`

- Add `temperature: Option<RawTemperature>` to `RawSmartctlOutput` (line 11-21) with a `RawTemperature { current: Option<i32> }` helper struct. Deserialize as `i32` and narrow to `i16` at the boundary (temps in Celsius comfortably fit in `i16`; `i32` is just what smartctl tends to emit as JSON numbers).
- Define `SmartProbe { pub health: SmartHealth, pub celsius: Option<i16> }` in `cli/src/parse/types.rs` next to `SmartHealth` (line ~355).
- Rename `parse_smartctl_health` to `parse_smartctl` returning `SmartProbe` (no back-compat per AGENTS.md "no backwards compatibility").
- Update all existing tests in `cli/src/parse/smartctl.rs:174-336` to assert on `.health` instead of the bare enum, and add new cases:
  - SATA JSON with `temperature.current` present → `celsius == Some(N)`.
  - JSON with no `temperature` block → `celsius == None`.
  - JSON with `temperature` object but no `current` field → `celsius == None`.
  - Unparseable input → `celsius == None`.

### 2. Parser golden test with a committed fixture (DONE)

Fixture `cli/tests/fixtures/nixos-25.11/smartctl-sata-with-temperature.json` captured from a physical Seagate ST500LM021 SATA drive on the Silverstone NAS (serial replaced with placeholder `00000000`). Contains `smart_status.passed: true`, SMART attribute 194 (Temperature_Celsius), and the top-level `temperature.current: 26` block.

Golden test `golden_smartctl_sata_with_temperature` in `cli/tests/support/golden_common.rs` asserts `health == Healthy` and `celsius == Some(26)`. Uses the "manual" test pattern rather than the `golden_test!` macro because `parse_smartctl` returns `SmartProbe` directly (infallible), not `Result`.

NVMe fixture not captured — low priority since the NVMe code path is identical at the parser level (both surface `temperature.current`); revisit if NVMe behavior ever diverges.

### 3. Probe: thread temperature + physical id through

File: `cli/src/tui/probe.rs:85-89`

- Call `parse_smartctl` instead of `parse_smartctl_health`.
- Before the per-disk loop, build a `disk_name -> LuksUuid` map from `domain.devices` the same way `devid_to_name` is built at line 48-52 (use `crate::config::name_from_mapper(&d.mapper.0)` to pair each device's mapper with its UUID field).
- For each disk with `Some(celsius)`, build a `TemperatureReading { id, celsius }`, where `id = TemperatureDiskId::LuksUuid(uuid)` if the UUID map has the disk, else `TemperatureDiskId::ByIdPath(by_id_path.clone())`. Insert into a `disk_temperature_readings: HashMap<String, TemperatureReading>` keyed by config name.
- Store alongside the existing `smart_health` on the constructed `PoolState`.

### 4. Model: PoolState gains readings, Model gains watermarks

File: `cli/src/tui/model.rs`

- On `PoolState` (line 97-117): `pub disk_temperature_readings: HashMap<String, TemperatureReading>`.
- On `Model` (line 143-159): `pub session_temperature_stats: HashMap<TemperatureDiskId, TemperatureWatermark>`, initialized empty.
- Define `TemperatureDiskId`, `TemperatureReading`, `TemperatureWatermark` in `cli/src/tui/model.rs` (or a small sibling module if that's cleaner — match the file's existing conventions). `TemperatureDiskId` should derive `Eq + Hash + Clone`.

### 5. App: watermark update + reset message

File: `cli/src/tui/app.rs`

- Add `Message::ResetTemperatureStats` to the `Message` enum (line 9-34).
- In `update()` (line 36-142):
  - `Message::PoolProbeFinished` arm (line 123-140): update `model.session_temperature_stats` from the **owned** `pool` first (iterate `pool.disk_temperature_readings.values()`, seed on first sample / widen min/max and bump `sample_count` on later samples), *then* move `pool` into `model.pool`. Doing this in the other order forces a borrow-check juggle.
  - Add `Message::ResetTemperatureStats` arm: `model.session_temperature_stats.clear()`.

### 6. Keymap: bind Shift+R globally

File: `cli/src/tui/keymap.rs`

- Add uppercase `R` binding *after* the `show_help` branch (line 7-9) and *before* the `show_disk_detail` branch (line 10-17). Match on `KeyCode::Char('R')` (which is what crossterm delivers when Shift is held with an alpha char). This placement preserves help-close-on-any-key and lets `R` work in main + disk-detail views.
- Keep lowercase `r` with its current `RefreshPool` semantics in both the disk-detail branch (line 14) and the main match (line 23).

### 7. View: new column + footer segment + helper

File: `cli/src/tui/view/mod.rs`

- Add a `temperature_cell(reading: Option<&TemperatureReading>, stats: &HashMap<TemperatureDiskId, TemperatureWatermark>) -> Line` helper next to the existing `smart_cell()` helper at line 312-318, implementing the render rule from the Design section.
- `disk_table()` (line 357-485):
  - Header (line 363): insert `"Temp"` between `"SMART"` and `"btrfs"`.
  - Row build (line 388-400): call `temperature_cell(pool.disk_temperature_readings.get(name), &model.session_temperature_stats)`.
  - Column widths (line 472-479): add a `Constraint` for the new column — `38° 32/45` is ~9 visible chars; budget 10-11 with padding.
- Footer (line 913-930): extend the `format!` string to include `Reset temp hi/lo: R`.

### 8. Tests

Snapshot rendering (`cli/src/tui/view/mod.rs` test module + `cli/src/tui/view/snapshots/*.snap`):
- Existing snapshots drift as soon as the column is added. Run `cargo test --lib tui::view`, review diffs, accept via `cargo insta review`.
- Add one new snapshot test exercising all three render branches in a single frame: one drive with `sample_count >= 2` (`38° 32/45`), one with `sample_count == 1` (`38° --/--`), one with no reading (`-`).

App behavior (`cli/src/tui/app.rs` test module):
- `PoolProbeFinished` first-sample test: empty `session_temperature_stats`, one reading arrives, verify entry is seeded with `min_celsius == max_celsius == reading.celsius`, `sample_count == 1`.
- `PoolProbeFinished` subsequent-sample test: preseeded entry, new reading higher than `max_celsius`, verify `max_celsius` widens, `min_celsius` unchanged, `sample_count == 2`. Mirror for lower-than-`min_celsius`.
- `PoolProbeFinished` missing-reading test: preseeded entry, probe returns a PoolState with no reading for that disk, verify the watermark entry is untouched (no stale-current effect).
- `ResetTemperatureStats` test: populated map, dispatch `ResetTemperatureStats`, verify map is empty.

Keymap (`cli/src/tui/keymap.rs` test module, extending the existing one):
- Uppercase `R` in main mode → `Some(Message::ResetTemperatureStats)`.
- Uppercase `R` in disk-detail mode → `Some(Message::ResetTemperatureStats)`.
- Uppercase `R` in help mode → `Some(Message::ToggleHelp)` (help-close-on-any-key wins; `R` does not reset from inside help).
- Lowercase `r` continues to return `Some(Message::RefreshPool)` in main and disk-detail (regression guard).

## Verification

1. `cargo check -p braid` — type-checks the refactor.
2. `just test-rust` — inline-JSON parser tests (Step 1), app update/reset tests, keymap tests, and view snapshot tests all pass. The fixture-backed golden test from Step 2 is **not** part of this run until the real capture lands.
3. `just test-parsers` — existing stable parser lane still green (no invocation change, just extracting an extra field).
4. Manual: launch the TUI against a real pool or a VM test with `virtualisation.emptyDiskImages`. Expected sequence on a working drive:
   - First tick with a valid `temperature.current`: cell shows `38° --/--`.
   - Second tick: cell shows `38° 38/38` if the value didn't move, `38° 37/38` if it did, etc.
   - Press `Shift+R`: watermarks reset, cell returns to `38° --/--` on the same tick (current reading preserved).
   - Press `r`: pool reloads, watermarks unaffected (only the underlying probe refreshes).
   - Open disk-detail with Enter, press `Shift+R`: watermarks still reset.
   - Footer continuously shows `Reset temp hi/lo: R`.
5. `just test-vm` — no VM tests need changing (TUI isn't VM-tested), but run to confirm nothing broke.

## Non-goals

- No persistence of watermarks across TUI restarts.
- No per-disk reset (global only).
- No sparklines, no details pane, no color coding based on thresholds. All future work.
- Not using `smartctl -l scttempsts` or device statistics log — drive-lifetime min/max is the wrong semantic for this feature.

## Critical files

- `cli/src/parse/smartctl.rs` — parse changes + unit tests
- `cli/src/parse/types.rs` — add `SmartProbe`
- `cli/src/tui/probe.rs` — call new parser, build `disk_temperature_readings` with physical id
- `cli/src/tui/model.rs` — `TemperatureDiskId`, `TemperatureReading`, `TemperatureWatermark`, `PoolState.disk_temperature_readings`, `Model.session_temperature_stats`
- `cli/src/tui/app.rs` — `Message::ResetTemperatureStats`, watermark fold in `PoolProbeFinished` arm, tests
- `cli/src/tui/keymap.rs` — uppercase `R` binding placed after help guard, before disk-detail guard; tests
- `cli/src/tui/view/mod.rs` — header, row, column width, `temperature_cell()` helper, footer, snapshot test
- `cli/src/tui/view/snapshots/*.snap` — regenerate all drifted snapshots, add one new snapshot
- `cli/tests/fixtures/nixos-25.11/smartctl-sata-with-temperature.json` (new) — fixture for golden parser test
