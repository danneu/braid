# Split per-disk error reporting into `btrfs_errors` + `smart`

GitHub issue: https://github.com/danneu/braid/issues/27

## Context

`braid status` reports btrfs device errors per disk but nothing about SMART;
the only SMART signal it surfaces is a global smartd alert flag. The TUI shows
SMART solely as a bare health enum (`ok`/`warning`/`failing`) in a column, with
no way to see *why* a drive is degraded and no btrfs/SMART consistency. SMART
health is computed in `parse_smartctl` (`cli/src/parse/smartctl.rs`) and then
**discarded** -- `classify_sata`/`classify_nvme` read the underlying counters
(reallocated/pending/uncorrectable sectors; media errors, wear, spare) only to
collapse them to a `SmartHealth` enum.

This change surfaces SMART evidence alongside btrfs errors across all three
surfaces, as two explicitly-named concepts (btrfs error counters and SMART
health), because they are observations from different layers (the filesystem's
I/O accounting vs. the drive's self-report) and should not share a vague
"Errors" label.

## Locked design decisions (from design discussion)

- **Two named concepts, not one merged "Errors":** `--json` field `errors`
  renames to `btrfs_errors`; new sibling object `smart`.
- **`smart` is a verdict + evidence, not a flat count.** SMART's authoritative
  signal is a pass/fail verdict; counts are supporting evidence. A single summed
  `smart_errors` integer was rejected (mixes units; would render `0` on a drive
  reporting `passed:false`).
- **`protocol` discriminator** (`sata`/`nvme`) so the evidence field set is
  unambiguous and the shape is forward-compatible.
- **NVMe is fully implemented**, not TODO'd -- the counters are already parsed
  (`RawNvmeHealth`), `media_errors` is a clean headline parallel to SATA
  `reallocated_sectors`, and tests are the same hand-authored pattern.
- **`status` probes `smartctl` plainly (no `-n standby`).** braid does only
  whole-system suspend-to-RAM (no per-drive spindown; see
  `docs/guides/power-management.md`), so whenever `status` can run, drives spin.
- **`celsius` ships in the `--json` `smart` object** but is NOT shown in the TUI
  detail section (it has its own Temp column; it is not a verdict input).
- **TUI column stays the bare health verdict (unchanged).** Error evidence lives
  in the per-disk detail panel as a new `SMART` section, sibling to the existing
  `btrfs Device Errors` section.
- **Per-disk `smart` is diagnostic evidence only -- it does not feed the alert
  latch.** The "SMART health warning" alert cause stays `AlertCause::SmartdAlert`
  (smartd-flag-driven via `/var/lib/braid/smartd-alert`; see ADR 014); a live
  `smart.health == "warning"` from the new per-disk probe must never synthesize an
  `AlertCause`. So `status` can show a degraded `smart` object while
  `alert_active` is `false` -- this is intentional and must be documented so the
  two SMART signals are not conflated.

## Target JSON shape (per disk, in `DiskReport`)

```json
"btrfs_errors": { "read": 0, "write": 0, "flush": 0, "corruption": 0, "generation": 0 },
"smart": { "health": "warning", "protocol": "sata",
           "reallocated_sectors": 2, "pending_sectors": 0, "offline_uncorrectable": 0,
           "celsius": 41 }
```

NVMe: `{ "health":"ok", "protocol":"nvme", "media_errors":0, "critical_warning":0,
"percentage_used":12, "available_spare":100, "available_spare_threshold":10, "celsius":52 }`.
No SMART: `{ "health": "unknown" }`. Both objects use
`skip_serializing_if = "Option::is_none"` and are omitted for disks with no data
(missing/offline), mirroring today's `errors`.

## 1. Data model -- `cli/src/parse/types.rs`

Currently `SmartProbe { health, celsius }` and `SmartHealth` derive neither
`Serialize` nor `Deserialize` (blocker for `--json`).

- `SmartHealth`: add `Serialize, Deserialize` with explicit per-variant renames
  so the JSON strings match the TUI words: `Healthy->"ok"`, `Degraded->"warning"`,
  `Failing->"failing"`, `Unknown->"unknown"`.
- New `SmartEvidence` enum, internally tagged `#[serde(tag = "protocol",
  rename_all = "snake_case")]`, `Copy`:
  - `Sata { reallocated_sectors, pending_sectors, offline_uncorrectable }`
  - `Nvme { media_errors, critical_warning, percentage_used, available_spare, available_spare_threshold }`
- `SmartField` enum: a stable per-field identity (SATA `Reallocated` / `Pending`
  / `Uncorrectable`; NVMe `CriticalWarning` / `MediaErrors` / `AvailableSpare` /
  `PercentageUsed`) with `fn label(self) -> &'static str`. Decouples a field's
  identity from its rendered text so the TUI red row, the human parenthetical, and
  the color test all key off the *enum*, never a matched string.
- `SmartEvidence::fields(&self) -> Vec<(SmartField, u64, bool)>`: every display
  field as `(key, value, is_concern)` -- the **single source** of both the shown
  value and the per-protocol "out of spec" test. `is_concern` predicates: SATA
  `reallocated > 0` / `pending > 0` / `uncorrectable > 0`; NVMe
  `critical_warning != 0` / `media_errors != 0` / `available_spare_threshold > 0 &&
  available_spare <= available_spare_threshold` (a threshold *pair*, not a `> 0`
  rule -- exactly why a generic numeric rule is wrong for NVMe; the threshold is
  consulted here but is not its own row) / `percentage_used >= 90`.
- `SmartEvidence::concerns(&self) -> Vec<(SmartField, u64)>` = the `is_concern`
  subset of `fields()`. Drives the SATA/NVMe verdict (`concerns().is_empty() ?
  Healthy : Degraded`, section 2) and the human `SMART:` parenthetical (section 3).
- One structure feeds all three surfaces: the TUI builds each row from `fields()`
  and reds it **iff that field's `is_concern` bool is set** (never a label-string
  match), the human line lists `concerns()`, and both label via
  `SmartField::label()` -- so the column verdict, the human line, and the TUI rows
  share one threshold definition and one label per field and cannot disagree.
  `fields()`/`concerns()` do **not** change the `--json` shape: the serialized
  `smart` object still carries the full evidence field set (all 3 SATA / 5 NVMe
  fields, incl. `available_spare_threshold`); these are verdict/render helpers only.
- Extend `SmartProbe` (stays `Copy`): add
  `evidence: Option<SmartEvidence>` and serialize the evidence flat
  (`#[serde(flatten, skip_serializing_if = "Option::is_none")]`) so `protocol` +
  counters appear at the `smart` object level; `celsius` gains
  `skip_serializing_if`.
  - The `flatten` + internally-tagged `Option<SmartEvidence>` combination
    round-trips on the pinned serde (`1.0.228` / serde_json `1.0.150`) -- verified
    empirically for SATA, NVMe, and unknown -- so no hand-written
    `Serialize`/`Deserialize` is needed; the serialization unit test (section 5)
    locks the exact shape.

## 2. Parser -- `cli/src/parse/smartctl.rs`

- Factor protocol detection into a helper usable regardless of pass/fail (today
  it is only inspected on the `passed:true` path; protocol + evidence must be
  derivable on the `Failing` path too).
- Add `sata_evidence(&RawAtaSmartAttributes) -> SmartEvidence` and
  `nvme_evidence(&RawNvmeHealth) -> SmartEvidence`, reusing the exact field reads
  from the old `classify_sata`/`classify_nvme` (`Reallocated_Sector_Ct` masked
  `& 0xFFFF`, `Current_Pending_Sector`, `Offline_Uncorrectable`; the five NVMe
  fields).
- **Build `evidence: Option<SmartEvidence>` once, derive `health` from it (single
  source at the call level too).** In `parse_smartctl`:
  - no `smart_status` -> `health: Unknown`, `evidence: None`;
  - else build `evidence` from the protocol's source Option (`.as_ref().map(...)`,
    so `None` when that detail log is absent -- see next bullet);
  - `health` = `Failing` if `smart_status.passed == false`, else
    `evidence.map_or(Healthy, |e| if e.concerns().is_empty() { Healthy } else { Degraded })`.
  This **removes** the separate `classify_sata`/`classify_nvme`/`classify_health`
  verdict fns -- their thresholds now live once in `SmartEvidence::fields`
  (section 1). Safe and behavior-preserving: every existing smartctl test asserts
  through `parse_smartctl(&raw(json)).health` (none call `classify_*` by name), so
  the preserved contract is `parse_smartctl(...).health`; `concerns().is_empty()`
  is the De Morgan dual of the old `bad`/`degraded` booleans over identical field
  reads (incl. the SATA `& 0xFFFF` mask, now in `sata_evidence`, so
  `sata_healthy_reallocated_zero_with_nonzero_upper_bytes` still masks to `0` ->
  empty concerns -> `Healthy`).
- **`evidence: None` when the source detail log is absent -- not only when health
  is `Unknown`.** `nvme_evidence`/`sata_evidence` are called only when their source
  Option is `Some` (`parsed.nvme_smart_health_information_log` /
  `parsed.ata_smart_attributes`). Required because every `RawNvmeHealth` field is
  `#[serde(default)]` (-> `0`), and `0` is the *failure* value for
  `available_spare`: a passing-but-logless NVMe drive (USB-NVMe bridges; the old
  `classify_nvme` returned `Healthy` when the log was absent) must yield
  `evidence: None`, not `Nvme { available_spare: 0, ... }` that reads as total spare
  exhaustion. (SATA zero-fill is benign -- `0` reallocated/pending/uncorrectable is
  the *good* value -- but gate on `ata_smart_attributes` presence too, for
  symmetry.) `evidence` is thus `None` in two cases: no `smart_status` (Unknown), or
  `smart_status` present but the per-protocol detail log absent.
- `parse_smartctl` returns `SmartProbe { health, celsius, evidence }`.
- The old `// TODO: validate with real SATA fixture` (was in `classify_sata`,
  now folded into `sata_evidence`) is partly retired by the extended golden
  assertion (below).

## 3. `status` -- `cli/src/status.rs`, `cli/src/cmd.rs`

- Reuse the existing `CmdRequest::SmartctlHealthJson { device }` variant
  (`smartctl -H -A <device> --json`, already used by the TUI) and `parse_smartctl`.
  No new command plumbing.
- `build_disk_reports` builds **two paired structs per disk** in lock-step:
  `DiskReport` (the `--json` surface) and `HumanDisk` (the only struct
  `format_status_human` reads -- it does **not** render from `DiskReport`). The
  probe must feed both, so compute it **once** per disk and hand the same value to
  each push:
  - **Present-device loop** (`status.rs#build_disk_reports`, the push pair that
    sets `errors`/`devid: Some`): probe
    `runner.run(&CmdRequest::SmartctlHealthJson { device: pd.underlying })` then
    `parse_smartctl`. **Failure-tolerant:** any `Err`/empty ->
    `SmartProbe { health: Unknown, celsius: None, evidence: None }` (mirror the
    TUI's `unwrap_or`). This makes the new call a no-op for the ~20 existing tests
    whose `MockRunner` returns `CmdError::MissingMock`. Target `pd.underlying` (the
    live `/dev/sdX`), not `by_id`. `SmartProbe` is `Copy`, so feed the same value
    into both `DiskReport.smart` and `HumanDisk.smart` with no clone.
  - **Unpooled/offline loop** (the push pair that already sets `errors: None`,
    `devid: None`, `underlying: None`): `smart: None` in both structs -- no backing
    path to probe.
- `DiskReport`: rename `errors` -> `btrfs_errors`; add
  `smart: Option<SmartProbe>` (both `skip_serializing_if = "Option::is_none"`).
- `HumanDisk`: add a matching `smart: Option<SmartProbe>` field (no serde attrs --
  this struct is render-only, never serialized). Without it the human `SMART:` line
  has no data source and will not compile.
- Human text (`format_status_human`, the per-disk block): relabel the existing
  `Errors:` line to `btrfs:` so it parallels the new line and is unambiguous now
  that two error-ish lines sit adjacent (the `--json` key is `btrfs_errors`; the
  TUI section is `btrfs Device Errors`), then add a per-disk `SMART:` line after
  it, rendered from `HumanDisk.smart` -- `ok` / `warning (...)` / `failing (...)` /
  `unknown`, with the same missing/offline fallbacks (matched on `d.status`) the
  existing block already uses. The parenthetical is driven solely by
  `evidence.concerns()` being non-empty -- **not** by the verdict word -- so both
  `warning` *and* `failing` carry it when there is evidence to show, listing the
  `concerns()` pairs as `{value} {key.label()}` (e.g. `warning (2 reallocated)` for a
  degraded SATA drive, `failing (5 reallocated)` for a `passed:false` SATA drive whose
  attributes are non-nominal, `warning (92 percentage used)` for NVMe wear). Empty
  concerns -- a `failing` verdict with no braid-read attribute out of spec, or
  `ok`/`unknown` -- render bare, with no parenthetical, never a blanket nonzero-field
  dump. The relabel touches
  existing human-text test assertions on `"Errors:"` (see the test Audit bullet).

## 4. TUI -- `model.rs`, `probe.rs`, `view/mod.rs`, `demo.rs`, `browse/state.rs`

- `model.rs`: replace `PoolState.smart_health: HashMap<String, SmartHealth>`
  with `smart: HashMap<String, SmartProbe>` (carry the whole probe; single
  per-disk source instead of a second parallel map).
- `probe.rs` (`probe_pool_for_tui` SMART loop): insert the whole `SmartProbe`
  into the new map instead of just `probe.health`. Temperature handling
  unchanged.
- `view/mod.rs`:
  - `smart_cell` / disk-table column: read `smart.get(name).map(|p| p.health)`
    -- one-line change, column output **unchanged**.
  - `view_disk_detail`: add a `SMART` section immediately after the
    `btrfs Device Errors` section, same idiom (a `Table` with `Borders::TOP`, cyan
    title, label/value `Row`s). Build one `health` row plus one row per
    `evidence.fields()` triple `(key, value, is_concern)`. The `health` row's value
    is styled by the **shared `SmartHealth` -> `Color` severity mapping** the disk
    table already uses (`Failing` -> red, `Degraded` -> yellow, `Healthy`/`Unknown`
    -> dark-gray) -- factor that arm out of `smart_cell` into a `smart_health_color`
    helper and call it from both, so the column and the detail verdict cannot
    diverge (column output stays byte-identical -- the extraction is a pure refactor).
    This makes a `failing` drive with **no** concern rows (e.g. `evidence: None`)
    still show a red `health` row matching its red column cell, instead of an
    all-uncolored detail section that understates the strongest signal. The evidence
    rows take label from `key.label()`, value from `value`, **red style iff
    `is_concern`** -- read straight from the field's bool, never a label-string
    membership test. This is
    the NVMe-inversion fix: a healthy NVMe (`available_spare 100`,
    `percentage_used 12`) has `is_concern == false` on those rows so they render
    *un-colored*, and a wear-degraded NVMe (`percentage_used 92`) reds only that
    row. No `temperature` row. `evidence: None` (`health: unknown`, or a logless
    drive) -> single `health` row. Add its height to the panel layout calc.
- `demo.rs` (`sample_pool` / `sample_disk_names`): seed the new `smart` map so the
  regenerated snapshots exercise both protocols and the evidence-row path:
  - **Index-0 disk (`toshiba`)** -- which `snapshot_disk_detail` renders by default
    (`new_demo` sets `selected_disk: 0`; `sample_disk_names()` is
    `[toshiba, ironwolf, wdc]`) -- gets a **degraded-SATA** probe (`reallocated >
    0`), with its btrfs errors staying `0`. This makes the *headline*
    `snapshot_disk_detail` cover a red SATA evidence row and demonstrates the
    btrfs/SMART independence (clean btrfs + `warning` SMART on one disk). Without
    seeding index 0, the default snapshot renders an all-`ok` SMART section and
    never exercises an evidence row.
  - Add a **4th NVMe demo disk** (transport `nvme`) with a degraded-NVMe probe
    (e.g. `percentage_used >= 90` and/or `media_errors > 0`) and a new
    `snapshot_disk_detail_nvme` test selecting its index, so a snapshot exercises
    the NVMe evidence rows (no NVMe path is visualized today). Adding the disk also
    churns every snapshot that renders the disk table/list (regenerate them). Keep
    `wdc` as the `Unknown`-SMART case (single `health`-row coverage).
- `browse/state.rs` (`#tests` `pool()` helper): mechanical `PoolState.smart_health`
  -> `smart` field rename in its test-only `PoolState` literal -- no behavior change
  (the edit itself is covered by the section 5 rename inventory; listed in this
  touched-file set so no renamed file is invisible to the plan).

## 5. Tests

- **Parser unit tests** (inline in `smartctl.rs#tests`, the existing `raw(json)`
  pattern): assert `health` + `evidence` for clean SATA (`Sata{0,0,0}`,
  `Healthy`), degraded SATA (`reallocated > 0`, `Degraded`), failing SATA
  (`passed:false` *with* attributes present -> `Sata` evidence + `Failing`), NVMe
  healthy (`available_spare 100` / `percentage_used 12` -> `Healthy`), NVMe wear
  (`percentage_used >= 90` -> `Degraded`), NVMe media-errors (`media_errors > 0`
  -> `Degraded`), NVMe low-spare (`available_spare <= available_spare_threshold`
  -> `Degraded`), and unknown (no `smart_status` -> `evidence: None`).
- **`fields()`/`concerns()` unit tests** -- the structure-insensitive lock on the
  High/Medium findings (the real semantic guard, independent of any rendered
  bytes). For each `SmartEvidence` case assert the exact `concerns()` set **by
  `SmartField` key**, critically: healthy NVMe (`available_spare 100`,
  `available_spare_threshold 10`, `percentage_used 12`) -> `concerns()` **empty**
  and `fields()` carries `is_concern == false` on the spare/used rows (proves no
  false-positive red on a healthy drive); NVMe wear -> `[(PercentageUsed, 92)]`;
  NVMe low-spare -> includes `(AvailableSpare, _)`; clean SATA -> empty; degraded
  SATA -> `[(Reallocated, _)]`.
- **Logless-drive tests** (F2): a passing NVMe JSON with **no**
  `nvme_smart_health_information_log` -> `health: Healthy`, `evidence: None` (not a
  zero-filled `Nvme{ available_spare: 0 }`); a SATA JSON with no
  `ata_smart_attributes` -> `evidence: None`.
- **Golden** (`cli/tests/golden_nixos_26_05.rs`): extend
  `golden_smartctl_sata_with_temperature` to also assert
  `evidence == Some(Sata{0,0,0})` against the real captured fixture. No new
  fixture files (VMs emit no SMART; degraded/NVMe shapes are covered by inline
  synthetic tests, matching the stable-only/hand-authored smartctl fixture
  policy).
- **Serialization contract** (new unit test): `serde_json::to_value` on a
  `SmartProbe` for SATA / NVMe / unknown, asserting the exact `smart` shape above
  (locks the contract since no `status --json` golden exists).
- **Status wiring + alert decoupling** (`cli/src/test_fixtures/status.rs` +
  `MockRunner`): mock a live `SmartctlHealthJson` response with **no** smartd-alert
  flag and assert: (a) `DiskReport.smart` is populated (and the paired
  `HumanDisk.smart`, via the human text), (b) the rendered human text includes the
  `SMART:` line, and (c) `report.alert_causes` (the `Vec<AlertCause>` on
  `StatusReport`) contains no `AlertCause::SmartdAlert`. **Table-drive (c) over both
  `Degraded` *and* `Failing` live probes** (`reallocated > 0` -> `warning`;
  `passed:false` -> `failing`) -- not just the degraded case -- so the lock also
  catches a future regression that wires the alert latch to `SmartHealth::Failing`
  specifically (the most tempting variant to escalate). (a)/(b) can stay on the
  single degraded probe. (c) is the structure-insensitive lock on the locked decision
  above: live per-disk SMART must never synthesize an alert cause, at any severity.
- **Human SMART line -- `failing` with evidence** (the structure-insensitive lock on
  the verdict-independent parenthetical): render a disk whose probe is
  `{health: Failing, evidence: Some(Sata { reallocated > 0, .. })}` and assert the
  human `SMART:` line reads `failing (N reallocated)` -- the concern parenthetical
  must appear for a `failing` verdict, not only `warning`. Reuses the `passed:false`-
  with-attributes shape the parser test already builds.
- **Audit** existing `build_status` tests for any that assert a whole serialized
  report (would now include `btrfs_errors`/`smart`); update expectations. Two
  rename sweeps:
  - `--json` key: the field rename `errors`->`btrfs_errors` touches any test
    asserting that JSON key.
  - Human text: the `Errors:`->`btrfs:` label relabel touches the existing
    assertions on `"Errors:"` / `"Errors:  read 0"` (several in
    `status.rs#tests`); update them to `"btrfs:"` and add the parallel `SMART:`
    assertions.
  - Rust field-access callsites -- compiler/test-enforced (loud, never silent), so
    this is an **open category, not a closed list**: rename every `errors:` /
    `smart_health:` struct-literal and every `.errors` / `.smart_health` access the
    compiler flags, wherever it points. Derive the inventory from `rg '\.errors\b'
    cli/src/status.rs` and `rg smart_health cli/src/` and re-run both after the sweep
    as verification (AGENTS.md rename-hygiene rule). Named explicitly because they are
    easy to miss:
    - `errors` -> `btrfs_errors` (on `DiskReport` only): in `status.rs#tests` --
      `assert_error_stats_retained` (`&BuiltStatus`, asserts `disk.errors.is_some()`;
      lives in `status.rs`, **not** the TUI), the two `disk.errors.is_none()`
      device-stats tolerance asserts, and the `ctx.disks[0].errors` read in the
      `build_disk_reports` devid-pairing test. **Excluded:** `HumanDisk.errors` and its
      `format_status_human` `d.errors` read keep the `errors` name -- only the
      serialized `DiskReport` field renames.
    - `PoolState.smart_health` -> `smart`: every `PoolState` literal/read --
      `tui/probe.rs` (`probe_pool_for_tui` builder + `#tests`
      `pool.smart_health.get("toshiba")`), the two `tui/view/mod.rs` column callsites
      (`smart_cell`, `smart_width`), `demo.rs` (`sample_pool`), and
      `tui/browse/state.rs#tests` `pool()` (`smart_health: HashMap::new()`).
- **TUI SMART-row coloring** (style-level, the behavioral lock on both the
  `is_concern`-driven evidence red *and* the verdict-severity `health` row). Text
  snapshots are blind to color -- `buffer_to_string` emits only `c.symbol()`
  (`tui/test_support.rs#buffer_to_string`) -- so a miscolor is invisible to insta.
  Read cell **style** from the `TestBackend` buffer
  (`terminal.backend().buffer().cell((x, y))` -> its `Style`/fg -- the same buffer
  `buffer_to_string` walks, inspecting style instead of glyph). **Locate cells
  structure-insensitively:** do **not** hardcode `(x, y)` off the row layout; scan
  the rendered buffer for the target row by its label/value text and assert style on
  the matched value cell, so a harmless popup-layout shift (an added row, a border
  tweak) does not break the test. Two render inputs:
  - **Degraded-NVMe detail** (locks `is_concern` -> red): the concerning row's value
    cell fg is `Color::Red`, and a non-concerning row's (a healthy
    `available_spare`/`percentage_used`) is not.
  - **`failing` / `evidence: None` detail** (locks F2: the verdict reaches the detail
    even with zero concern rows): construct a disk whose probe is
    `{health: Failing, evidence: None}` (inline test state -- no new demo disk, so no
    snapshot churn) and assert the `health` row's value cell fg is `Color::Red`,
    matching its column cell.
  Together these guarantee both `fields()`'s `is_concern` and the `SmartHealth`
  verdict actually reach the screen as color.
- **TUI snapshots** (row *text/layout* only -- coloring is locked by the
  style-level test above, since `buffer_to_string` drops style): regenerate
  `snapshot_disk_detail` (now exercises the seeded index-0 degraded-SATA evidence
  row), `snapshot_disk_detail_unmounted_mixed`, `snapshot_disk_detail_null_underlying`,
  the new `snapshot_disk_detail_nvme`, and any disk-table/list snapshot churned by
  the added 4th demo disk, via `cargo insta` after seeding `demo.rs`.

## 6. Docs

- `docs/commands/status.md`: document the `errors`->`btrfs_errors` rename and the
  new `smart` object (`--json` schema + the `btrfs:`/`SMART:` text lines). At the
  `## Alerts` section, state explicitly that per-disk `smart` is **diagnostic
  evidence only** and does not feed the alert latch -- the "SMART health warning"
  alert cause remains `AlertCause::SmartdAlert` (smartd-flag-driven), so a
  `smart.health == "warning"` report can coexist with `alert_active: false`. This
  reconciles the new field with the existing alert-causes copy (`status.md`
  currently lists "SMART health warnings" among alert causes -- that refers to the
  smartd latch, not the live probe).
- `docs/commands/tui.md`: update the **Disk detail popup** bullet -- it currently
  ends "...and the btrfs device-error breakdown (read/write/flush/corruption/
  generation)"; extend it to also list the new `SMART` section. The **Disk table**
  bullet stays accurate (the SMART-health column is unchanged), so leave it.
- `README.md`: update if it shows `status` output (per the README/docs sync rule).
- **Docs sweep -- the live SMART classifier is no longer TUI-only.** The stale
  "SMART classifier is TUI-only / only the TUI surfaces it" framing lives in two
  docs (**not** `AGENTS.md` -- it carries no parser list, only a pointer to
  `parser-compatibility.md`, so there is nothing to edit there):
  - `docs/dev/parser-compatibility.md` (the Stable-lane "TUI-only parsers" bullet,
    which names `parse_smartctl_health` -- a stale name; the real fn is
    `parse_smartctl`): after this change the smartctl health parser is reachable
    from the `status` CLI command, so it is no longer TUI-only. Move it out of the
    "TUI-only parsers" list and fix the `parse_smartctl_health` -> `parse_smartctl`
    name, noting it is now `status`-reachable but **still not** live-VM-canary-
    covered (virtio emits no SMART, so `just test-parsers` still cannot exercise it;
    the stable-only smartctl-fixture caveat in the same file is unchanged). Leave the
    Unstable-lane line ("...full parser surface (TUI-only parsers, unused parsers,
    smartctl)") as-is -- smartctl is already broken out there and stays uncovered by
    the live canary.
  - `docs/internals/tool-behavior/smartd-alerts.md` (`## Relationship to TUI
    `classify_sata``): both the heading symbol and the body are stale --
    `classify_sata` is removed (folded into `sata_evidence` / `SmartEvidence`,
    section 2), and "the TUI gives at-a-glance status" is now **status + TUI** (the
    same live probe also feeds `braid status`). Retitle the section around
    `SmartEvidence` and rewrite the body to name the status+TUI diagnostic path;
    keep the 3-attribute list (`Reallocated_Sector_Ct` / `Current_Pending_Sector` /
    `Offline_Uncorrectable` -- the exact reads `sata_evidence` preserves) and the
    "complementary to smartd" framing, which the alert-decoupling decision reinforces:
    the live classifier is diagnostic-only and smartd remains the alert source
    (ADR 014 / new ADR 030 below).
- New ADR `docs/design/decisions/030-smart-btrfs-error-reporting.md` (030 is the
  next free number -- 029 is the current highest; re-verify against
  `docs/design/decisions/` before writing). **Required frontmatter** (enforced by
  `scripts/docs/check-frontmatter.py`): a leading YAML block above the H1 with a
  non-empty `intent:` and `status: Active`, matching the shape of
  `022-dry-run-preview-model.md`. Capture: the `btrfs_errors`/`smart` split +
  `protocol` discriminator; verdict-plus-evidence (and why a flat `smart_errors`
  count was rejected); `status` now probes `smartctl` per disk and why that is safe
  (no per-drive spindown); column-summary vs detail-evidence split; and that
  per-disk `smart` is diagnostic only and does **not** feed `AlertCause`/the alert
  latch (smartd remains the alert source -- cross-reference ADR 014).
- `docs/SUMMARY.md`: add
  `- [030: SMART/btrfs error reporting](design/decisions/030-smart-btrfs-error-reporting.md)`
  to the decisions list, after the `029` entry. `check-docs` fails ("files missing
  from SUMMARY.md") for any `docs/**.md` file absent from the TOC, so the ADR file
  and this entry must land together.

## Verification

- `just test-rust` -- parser unit (`fields()`/`concerns()` by `SmartField` key) +
  logless-drive + golden + serialization + status wiring + **the style-level TUI
  coloring test** (the only check that sees red-vs-not, since snapshots drop style).
- `cargo insta review` (or the repo's insta flow) -- accept the regenerated
  disk-detail snapshots (`snapshot_disk_detail` now shows the index-0 degraded-SATA
  evidence *rows/text*; the new `snapshot_disk_detail_nvme` shows the NVMe rows)
  plus any disk-table snapshot churned by the 4th demo disk; confirm the row
  text/layout and that no temperature row appears. Coloring is **not** visible here
  (`buffer_to_string` drops style) -- it is locked by the style-level test under
  `just test-rust`.
- `just check-docs` + `just check-docs-frontmatter` -- the new ADR must be listed
  in `docs/SUMMARY.md` (membership comparison via `comm`) and carry a non-empty
  `intent:` + `status: Active` frontmatter, or these fail. Also run
  `mdbook build docs` (linkcheck) since the new `SUMMARY.md` link and any
  cross-links in `status.md`/`tui.md` are validated there.
- No new NixOS VM test: virtio disks emit no usable SMART, so live coverage is
  not possible (consistent with the stable-only smartctl fixture policy). The
  `smartctl -H -A` invocation reuses the path the TUI already exercises.
- Manual (real hardware only -- the only real SMART source): `braid status` and
  `braid status --json` show `btrfs_errors` + `smart` per disk; in the TUI,
  selecting a disk shows the new `SMART` detail section; a healthy drive reads
  `ok`, a drive with reallocated sectors reads `warning` in the column with the
  count in the detail.

## Risks / notes

- **`status` latency:** every `braid status` now spawns one `smartctl` per disk
  (synchronous). Accepted per the spindown analysis; affects only the CLI
  `status` path (not the monitor daemon -- `build_status` has no other caller --
  nor the TUI, which probes separately).
- **No-backwards-compat repo:** the `errors`->`btrfs_errors` rename is a hard
  break with no shim, by project policy.
- **Parser-compat:** `SmartProbe`/`SmartHealth`/`SmartEvidence` are now part of
  the serialized contract; the stable smartctl golden fixture remains the
  drift canary on smartmontools bumps.

## Implementation notes

- **`view_disk_detail` layout refactored to a dynamic section list.** Adding the
  `SMART` section as a third optional detail panel would have turned the existing
  4-arm `match (alloc_table, errors_table)` into an 8-arm match. Instead the three
  optional sections are collected into a `Vec<(Table, height)>` and the popup
  layout is built from that list. The new code reproduces the existing
  alloc/errors spacer+body layout byte-for-byte (verified: the non-SMART parts of
  regenerated snapshots changed only where the demo data changed) and simply
  appends SMART.
- **4th demo disk named `samsung` (NVMe); `ironwolf` set to a healthy
  SATA-with-evidence probe.** The plan pinned `toshiba` (degraded SATA), `wdc`
  (Unknown), and the new NVMe disk, but not `ironwolf`. `ironwolf` keeps its
  nonzero btrfs read count and now carries an `ok` SMART verdict, so the demo
  shows btrfs/SMART independence from both directions (toshiba: clean btrfs +
  `warning` SMART; ironwolf: `3 err` btrfs + `ok` SMART).
- **SMART detail table label column is width 16, not 15.** The btrfs Device
  Errors table uses 15, but the longest NVMe field label, `critical warning`, is
  16 chars; the SMART table column was widened to 16 so it renders without
  truncation.
- **Serialization test also asserts a Deserialize round-trip.** Section 5
  specified only a `to_value` shape lock, but the data-model section claimed the
  `flatten` + internally-tagged `Option<SmartEvidence>` round-trips on the pinned
  serde. The test now serializes then deserializes each of SATA/NVMe/unknown back
  and compares; it passes, confirming no hand-written `Serialize`/`Deserialize` is
  needed (including the bare-`{"health":"unknown"}` case).
