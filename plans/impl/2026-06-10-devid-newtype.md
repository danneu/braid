# Plan: introduce a `Devid` newtype for btrfs device ids

## Context

btrfs device ids (`devid`) are currently raw `u64` everywhere they appear: in
parser output, in domain types (`PoolState`, `PoolDevice`), in the alert/ack
pipeline, in membership (`pool.json`), and in `replace`. Because devid is just a
`u64`, the type system cannot distinguish it from the *other* `u64`s that sit
next to it -- most dangerously device **counts** (`missing_count`,
`total_devices`). A devid-vs-count mix-up compiles and silently corrupts pool
state reasoning.

This migration wraps devid in an opaque `Devid(u64)` newtype so the
devid-vs-count (and devid-vs-any-other-u64) axis becomes a compile error.

**Key finding that shaped the plan:** a single `Devid` newtype does **not** close
the two hazards the original brief called "most realistic" -- the
`recognized_devids`/`missing_devids` slice swap in `alert.rs#compute_alert_state`
and the `pool_devid`/`supplied_devid` transpose in
`replace.rs#ReplaceError::OldDevidMismatch`. Both operands are *devids*, so after
the migration they are both `Devid`/`&[Devid]` and remain swappable. The newtype
only separates devid from non-devid. The alert slice swap inverts alert/ack logic
on the operator-facing monitoring path, so we additionally **group the two alert
devid sets into one named input** to make the positional swap unwriteable.
(`OldDevidMismatch` is already a named-field struct literal; a transpose there
requires actively mislabeling a field -- adequate guard, no extra work.)

**Confirmed decisions:**
- **Swap hazard:** Devid newtype **+** group the alert slices.
- **Blast radius:** **natural flow** -- change the canonical type definitions and
  let the compiler carry `Devid` into every reader; convert back to `u64` only at
  genuine `u64`/FFI seams.

**Guarantees:**
- No serialized-format change. `Devid` is `#[serde(transparent)]`, so every JSON
  field/value/key keeps its current `u64` shape (`pool.json`'s `"devid":1`,
  `alert-latch.json`'s `{"type":"missing_device","devid":7}`, `--json` parser
  fixtures). No fixture regeneration.
- No production arithmetic on devids (verified: `add.rs:4351` and
  `replace.rs:2021` are test-fixture *string* synthesis on plain ints, not devid
  math). The newtype deliberately exposes no arithmetic.

## The `Devid` newtype

Define in `cli/src/types.rs` next to `MapperName`/`MountPoint` (matches the
transparent-serde house style; `parse/types.rs` already imports
`crate::types::LuksUuid`, so the parser layer can import `crate::types::Devid`
the same way). Private field like `DataRatio`/`LuksUuid` so `.0` arithmetic is
impossible by construction.

```rust
/// btrfs device id (devid). Opaque u64 wrapper: devids are identity, never
/// arithmetic, and must not be confused with device counts (missing_count,
/// total_devices) or other u64s on the same structs. Constructed only at the
/// parser/CLI-arg boundary; rendered via Display at argv and JSON-key seams.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Devid(u64);

impl Devid {
    /// Wrap a raw devid at a trust boundary (parser output, --missing-id arg,
    /// JSON-key parse-back). No validation: btrfs devids are arbitrary u64.
    pub fn new(raw: u64) -> Self { Devid(raw) }
    /// Unwrap to the raw u64 only at FFI/kernel-ABI seams that require it.
    pub fn get(self) -> u64 { self.0 }
}

impl fmt::Display for Devid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { self.0.fmt(f) }
}
```

`Copy` + `Display` + `Ord`/`Hash` are what make natural flow nearly free:
`format!("{devid}")`, `devid.to_string()`, `HashMap<Devid, _>`, `BTreeSet<Devid>`
all keep working unchanged.

## Production devid inventory (rg-derived)

Natural flow only holds if *every* production devid field/param becomes `Devid`;
any one left as `u64` is a fresh seam that silently accepts a count or unrelated
id. Inventory built from:

```
rg -n 'devid:\s*u64|devid:\s*Option<u64>|devids:\s*(Vec<u64>|&\[u64\]|Option<Vec<u64>>)|_devid:\s*u64|_devids:\s*(Vec<u64>|&\[u64\])' cli/src -g '*.rs'
```

Every site below is **production** and migrates to `Devid` / `Vec<Devid>` /
`Option<Devid>` / `&[Devid]`. The migration is **not done** until this list is
clear (the verification step greps for residual production `devid: u64`):

- **Parsers** (`parse/types.rs`, four parser bodies) -- step 1 above.
- **Domain types** (`types.rs`): `PoolState.missing_devids` (line ~440),
  `PoolDevice.devid` (~492), `NullUnderlyingDevice.devid` (~505) -- step 2.
- **Probe/alert** (`probe.rs`, `alert.rs`): `AlertPoolState.present_devids`/
  `.missing_devids` (~272/273); `AlertCause::{BtrfsDeviceErrors,MissingDevice}.devid`
  (~28/29); `compute_alert_state`/`snapshot_current` slice params (~105/106,
  ~175/176); `drop_acked_devid(devid)` (~212) and `drop_acked_for_devids(devids)`
  (~234); `ack.rs` local `missing_devids: Vec<u64>` (~150) -- steps 3-5.
- **Membership** (`membership.rs`): `DiskMember.devid: Option<u64>` (~245),
  `by_devid` param (~287), `DuplicateDevid.devid` (~49) -- step 6.
- **Replace** (`replace.rs`): `ReplaceParams.missing_id: Option<u64>` (~159, a
  devid named `missing_id`) and the resolver param `missing_id: Option<u64>`
  (~1821); `OldDevidMismatch.{pool_devid,supplied_devid}` (~130/131);
  `ReplaceSource::Live.devid`/`Missing.devid` (~1649/1651);
  `source_has_io_errors`/`format_source_io_error_warning`/
  `format_source_io_probe_failure` params (~1110/1123/1133); null-underlying
  refusal closure (~1863) -- step 7.
- **Command argv** (`cmd.rs`): `BtrfsReplaceStart.devid` (~203),
  `BtrfsFilesystemResize.devid` (~211) -- step 8.
- **Pool ops** (`pool.rs`): `DeviceIdentity.devid` (~27, drift detection, and
  **serialized** drift-mismatch rendering); `pool_replace_device(devid)` (~542)
  and its sibling resize fn (~566) -- step 8/10.
- **Preflight** (`preflight.rs`): `ReplaceSourceProbe.devid` (~431); the
  `total_bytes(mount, source.devid)` call is the FFI seam (step 9).
- **Journal** (`journal.rs`, **persisted JSON**): `OpKind`/`ReplaceJournalSource`
  variants `old_devid` (~120/133) and `devid` (~195) -- step 10.
- **Recover** (`recover.rs`): `RecoverCompletion::{RemoveMissingPoolMutation,
  RemoveMissingPostMaintenance}.devid` (~260/264) and other completion devid
  fields; replay/error variants `DuplicateDevidDuringReplay`,
  `NoMemberForJournaledDevid`, `DuplicateDevid`, `NoMemberForDevid` (~64/71/81/82);
  recovery-execution params (~2645/2721); `sweep_devids: Vec<u64>` local (~2620)
  -- step 10.
- **Remove / remove-missing** (`remove.rs`, `remove_missing.rs`):
  `RemoveWorkPlan.target_devid` (~133), `check_single_survivor(target_devid)`
  (~781), `RemoveConfirmDisk.devid` (~871); `remove_missing.rs#RemoveMissingParams.missing_id:
  u64` (~78, devid named `missing_id`), the remove-missing work-plan `missing_id`
  (~111), `validate_missing_id_target(pool, missing_id)` (~307, compares
  `d.devid == missing_id` / `pool.missing_devids.contains(&missing_id)` -- both
  sides must be `Devid`), `NoMemberForDevid` (~33), work-plan `devid` field (~67),
  exec params (~684/750) -- step 10.
- **Status** (`status.rs`): `DiskReport.devid: Option<u64>` (~209, **serialized**
  JSON status surface), `CompactDrive.devid` (~257, render-only), `missing_devids:
  Vec<u64>` (~86), `devid_to_name` + `HashMap<u64,String>` (~1289) -- step 10.
- **TUI identity** (`tui/model.rs`, `tui/probe.rs`): `DiskIdentity.devid:
  HashMap<String, u64>` (~65, devid as map **value**) -> `HashMap<String, Devid>`;
  `tui/probe.rs` `persisted_devid_to_name: HashMap<u64, &str>` (~164) and
  `devid_to_name: HashMap<u64, &str>` (~223), devid as map **key** ->
  `HashMap<Devid, &str>` (`Hash`/`Eq` derive) -- step 10.
- **Misc renderers**: `lock.rs#duplicate_devid_warn_body` (~324),
  `repair_hint.rs#missing_replace_command_with_devid` (~23) -- format via
  `Display`; param -> `Devid`.

**Two non-obvious disguises** the plain `devid:\s*u64` grep misses, so the
residual gate (verification #5) must also catch them:
1. **Devid named `missing_id`** -- the `--missing-id` value threads through
   `RemoveMissingParams`, `ReplaceParams`, work plans, and `validate_missing_id_target`
   as `missing_id: u64` / `Option<u64>`. All migrate to `Devid`.
2. **Devid in a map key/value position** -- `HashMap<u64, _>` /
   `HashMap<_, u64>` / `HashSet<u64>` / `BTreeSet<u64>` whose `u64` is a devid
   (status `devid_to_name`, TUI identity maps, membership dup-detection,
   monitor/alert `still_relevant` sets). These become `Devid`-typed; generic
   *count*/*size* maps stay `u64` (allowlist #3).

### Allowlist (stays `u64` -- justified, not an oversight)
1. **Kernel ABI struct**: `btrfs_ioctl.rs#BtrfsIoctlDevInfoArgs.devid` (~13,
   `#[repr(C)]` kernel contract). `for_devid` takes `Devid`, sets
   `.devid = devid.get()` -- the single `.get()` unwrap (step 9). The
   `BtrfsDevInfo::total_bytes` trait param and `DevidNotFound`/`IoctlFailed`
   error variants *do* become `Devid`; only the ABI field stays raw.
2. **Parser-local raw accumulators before conversion**: `parse/...` intermediate
   `Vec<u64>` / `RawDeviceStatsEntry.devid` / `PartialDevice.devid` (step 1) --
   raw until the one documented `Devid::new` at the map/finalize.
3. **Generic counts / sizes**: `missing_count`, `total_devices`, `total_bytes`,
   the btrfs error counters, `device_size`, `unallocated` -- this is the
   devid-vs-count line the newtype exists to draw; they must *not* become `Devid`.
4. **Test-only integer synthesis**: fixture/test helpers that compute devid ints
   in a loop or env-var smoke test (`add.rs:5332/5395`, `capacity.rs:110`,
   `btrfs_ioctl.rs:183` smoke `BRAID_BTRFS_IOCTL_SMOKE_DEVID`, the
   `next_devid`/`with_disk2_devid` loops). Keep the `u64` counter; wrap with
   `Devid::new(...)` at the struct literal they build. Test *helper signatures*
   that build a `Devid`-bearing struct (e.g. `target_validation_device`,
   `status_disk_report_named`, `disk_member_named`, `metadata_pressure_result_with_pool`'s
   `missing_devids: &[u64]`) take the `Devid`/`&[Devid]` type they construct.
5. **CLI clap args** (`main.rs` `missing_id: u64` ~347 and `missing_id: Option<u64>`
   ~362): the clap value-parser boundary stays raw `u64`; wrap with `Devid::new(...)`
   exactly where the args build `ReplaceParams`/`RemoveMissingParams`
   (`main.rs:633/674`). This is the production *construct* seam for user-supplied
   devids (parallel to the parser construct points in step 1).

## Milestones (resumable checkpoints)

This migration is **natural-flow**: flipping any canonical devid type (e.g.
`PoolDevice.devid`) breaks the whole crate until every consumer is fixed, so there
is **no green build inside the production cascade**. The milestones below are the
only points where the tree compiles and a check passes. A fresh agent stops at a
milestone's exit and resumes at the next using just this plan file plus the
milestone name; each milestone states (a) the *precondition* that proves the prior
milestone really landed and (b) the *exit* command that must pass before stopping.

Why these boundaries hold: `test_fixtures` and the inline `#[cfg(test)]` modules
are `#[cfg(test)]`-gated (`lib.rs`), and `cli/tests/` is a separate test crate --
all three are excluded from `cargo build`. That is what lets M1 (production) be a
green checkpoint while test literals are still raw `u64`; M2 brings the test trees
back. Do not hand off a non-compiling tree as a milestone -- a half-done cascade is
resumed by "keep flipping until green," not by committing red.

### M0 -- Newtype foundation (section "The `Devid` newtype")
- **Do:** add `Devid` to `types.rs` exactly as specified (full derive set,
  `new`/`get`, `Display`, the `///` doc comment) plus any needed `use std::fmt`.
  Nothing consumes it yet.
- **Exit:** `cargo build` green **and** `rg -n 'pub struct Devid' cli/src/types.rs`
  matches. The rest of the crate is still raw `u64` -- expected.
- **Note:** trivially combinable with M1 in one sitting; kept separate so a resumer
  has a confirmed, spec-matching type to build the cascade against.

### M1 -- Production cascade (steps 1-10)
- **Precondition (prove M0 landed):** `Devid` exists in `types.rs` with the full
  derive set and `cargo build` is green before you flip any field.
- **Do:** flip every production devid field/param/map in the inventory to `Devid`
  (steps 1-10), including the `AlertDevids` grouping (step 3) and the `btrfs_ioctl`
  internal unwrap (step 9). Work the existing step order -- it is already arranged
  so the compiler's error list is your worklist.
- **No interior stop:** the crate does not compile again until the cascade is
  complete. If you run out of context mid-cascade, recovery is "resume M1 and keep
  flipping until green" -- `cargo build`'s remaining devid-type-mismatch errors are
  the precise residual worklist. Never check in or hand off a red tree as a stop.
- **Exit (all must pass):**
  - `cargo build` (lib + bins; **not** `--all-targets`) green.
  - `cargo clippy` (lib + bins) clean.
  - Residual grep (verification #5, both passes): every **non-test** hit is an
    allowlist entry. Test trees are still raw at M1, so expect residual hits in
    `#[cfg(test)]` helper signatures and `cli/tests/` (e.g. `Vec<u64>` in
    `golden_common.rs`); those clear in M2. M1's bar is that no *production* hit
    remains.

### M2 -- Tests compile and pass (step 11)
- **Precondition (prove M1 landed):** `cargo build` green and the production
  residual grep clean.
- **Do:** update every test literal to `Devid::new(...)` across all three test
  trees -- inline `#[cfg(test)]` modules, `cli/src/test_fixtures/*`, **and the
  separate `cli/tests/` integration crate** (e.g. `tests/support/golden_common.rs`
  does `out.devices.iter().map(|d| d.devid).collect::<Vec<u64>>()` and
  `assert_eq!(devids, vec![1, 2])`; both the `Vec<u64>` annotation and the literal
  vec must become `Devid`). Mechanical, ~80-100 sites.
- **Exit:** `just test-rust` (or `cargo test` + `cargo clippy --all-targets`)
  green. No behavior-level new assertions yet -- this milestone only restores the
  existing suite.

### M3 -- New tests, format-stability, docs (step 12 + verification)
- **Precondition (prove M2 landed):** `just test-rust` green on the unchanged
  fixtures.
- **Do:** add the two new behavioral tests (alert-carrier semantics; `AlertState`
  JSON-shape exact-string), then run the full verification suite.
- **Exit:** verification items 1-7 all pass -- `just test-parsers`, every
  format-stability assertion (verification #4), **both** residual greps clean now
  that test trees are migrated (verification #5), the swap-hazard spot check
  (#6), and `just docs-build` if any doc references the alert signatures (#7).

## Migration steps (ordered so the compiler guides each stage)

### 1. Parser boundary (`cli/src/parse/types.rs` + the four parsers)
Change the public parser-output devid fields to `Devid`:
- `BtrfsShowDevice.devid` (line ~121)
- `BtrfsFilesystemShowOutput.missing_devids: Vec<u64>` -> `Vec<Devid>` (line ~132)
- `BtrfsDeviceUsageEntry.devid` (line ~288)
- `DeviceErrorStats.devid` (line ~329) -- **required** so `compute_alert_state`
  can compare stats rows against `Devid` sets
- `DeviceScrubEntry.devid` (line ~592)

**None of the four parsers deserialize serde-directly into these public structs**,
so the field-type change is *not* self-sufficient -- each has an explicit
conversion point that must wrap the raw `u64`. Verified shapes:

- `parse/btrfs_filesystem_show.rs#parse_btrfs_filesystem_show` -- **text** parser.
  `let mut missing_devids: Vec<u64>` accumulator (line ~113) stays raw; wrap into
  `Devid::new(...)` when pushing into / building `BtrfsFilesystemShowOutput`.
  Per-device devid likewise wrapped where `BtrfsShowDevice` is constructed.
- `parse/btrfs_device_stats.rs#parse_btrfs_device_stats` -- serde into a **private**
  `RawDeviceStatsEntry { devid: u64 }` (line ~22), then `.map(|e| DeviceErrorStats
  { devid: e.devid, .. })` (line ~53). Wrap at the map: `devid: Devid::new(e.devid)`
  (or type the private raw field as `Devid`, which makes serde-transparent apply
  there and the map a plain move). The public field change alone does nothing.
- `parse/btrfs_device_usage.rs#parse_btrfs_device_usage` -- **text** parser.
  `parse_u64` yields a `u64` into a private intermediate (`devid: u64`, line ~120),
  then maps into `BtrfsDeviceUsageEntry { devid: dev.devid }` (line ~143). Wrap at
  the map.
- `parse/btrfs_scrub_status_per_device.rs#parse_btrfs_scrub_status_per_device` --
  **text** parser. `parse_u64` -> `PartialDevice { devid: u64 }` (line ~69) ->
  `finalize()` builds `DeviceScrubEntry { devid: self.devid }` (line ~107). Wrap at
  `finalize`.

The parser-local raw accumulators (`Vec<u64>`, `RawDeviceStatsEntry.devid`,
`PartialDevice.devid`) are on the **allowlist** (stay `u64` until the single
documented conversion point) -- see the inventory's allowlist below.

### 2. Domain types (`cli/src/types.rs`)
- `PoolDevice.devid: u64` -> `Devid`
- `NullUnderlyingDevice.devid: u64` -> `Devid`
- `PoolState.missing_devids: Vec<u64>` -> `Vec<Devid>` (leave `missing_count` /
  `total_devices` as `u64` -- this is the devid-vs-count line the newtype draws)

### 3. Probe + the alert-slice grouping (`cli/src/probe.rs`)
- `AlertPoolState.present_devids` / `.missing_devids` -> `Vec<Devid>`; the
  `BTreeSet<u64>` unions in `recognized_devids()` / `alert_missing_devids()` ->
  `BTreeSet<Devid>`.
- **Grouping fix:** add a named carrier so the two devid sets are never two
  positional args. Add `AlertDevids { pub recognized: Vec<Devid>, pub missing:
  Vec<Devid> }` (in `alert.rs` or `probe.rs`) and a builder
  `AlertPoolState::alert_devids(&self) -> AlertDevids` that fills both fields.
- `probe_pool` (`PoolState` literal, ~line 483) and `probe_pool_alerts`
  (`AlertPoolState` literal, ~line 377) now flow `Devid` through unchanged.

### 4. Alert pipeline (`cli/src/alert.rs`)
- `AlertCause::BtrfsDeviceErrors { devid }` / `MissingDevice { devid }` -> `Devid`
  (serde-transparent: latch JSON unchanged).
- Replace the two positional slice params on `compute_alert_state` and
  `snapshot_current` with a single `&AlertDevids`. Internals: `BTreeSet<Devid>`
  for `recognized`/`missing`; `.contains(&dev.devid)` typechecks once
  `DeviceErrorStats.devid` is `Devid`.
- JSON keys stay strings: `devid.to_string()` keeps working via `Display`;
  read-back `key.parse::<u64>()` becomes `key.parse::<u64>().map(Devid::new)`
  (the `still_relevant` filter and `drop_acked_devid(devid: Devid)`).

### 5. Call sites (`cli/src/monitor.rs`, `cli/src/ack.rs`)
Replace the `let recognized_devids = ...; let alert_missing_devids = ...;` pairs
with `let devids = pool.alert_devids();` and pass `&devids`. `monitor.rs`'s
separate `still_relevant_devids: BTreeSet<u64>` reads `devids.recognized` and
becomes `BTreeSet<Devid>`.

### 6. Membership (`cli/src/membership.rs`)
- `DiskMember.devid: Option<u64>` -> `Option<Devid>` (serde-transparent:
  `"devid":1` unchanged; `skip_serializing_if` still applies).
- `by_devid(devid: u64)` -> `Devid`; `m.devid == Some(devid)` typechecks.
- `MembershipError::DuplicateDevid { devid }` -> `Devid`; the duplicate-detection
  `BTreeMap<u64, Vec<&LuksUuid>>` -> `BTreeMap<Devid, _>`.

### 7. Replace (`cli/src/replace.rs`)
- `ReplaceError::OldDevidMismatch { pool_devid: Devid, supplied_devid: Devid }`
  (thiserror `#[error("...{pool_devid}...{supplied_devid}...")]` renders via
  `Display`, message text unchanged).
- `ReplaceSource::Live { devid }` / `Missing { devid }` -> `Devid`.
- `ReplaceParams.missing_id: Option<u64>` -> `Option<Devid>`, and the resolver
  param `missing_id: Option<u64>` (~1821); `resolved`/`supplied` flow `Devid`.
- `--missing-id` CLI value: per allowlist #5 the clap arg stays raw `u64`; wrap
  with `Devid::new(...)` where `main.rs` builds `ReplaceParams` /
  `RemoveMissingParams` (`main.rs:633/674`), so `missing_id` and the downstream
  `supplied_devid` are `Devid` from the construct boundary inward. Mirror this for
  `remove_missing.rs#RemoveMissingParams.missing_id` and `validate_missing_id_target`
  (covered under step 10 / inventory).

### 8. Command argv (`cli/src/cmd.rs`)
- `CmdRequest::BtrfsReplaceStart.devid` and `BtrfsFilesystemResize.devid` ->
  `Devid`. The argv builders (`devid.to_string()`, `format!("{devid}:max")`)
  render via `Display` -- no change beyond the field type. (`--sort=devid,...`
  literals are column names, not values; leave them.)

### 9. FFI seam (`cli/src/btrfs_ioctl.rs`)
- `BtrfsDevInfo::total_bytes(&self, mount, devid: Devid)` and error variants
  (`DevidNotFound`/`IoctlFailed { devid: Devid }`). The private constructor
  `BtrfsIoctlDevInfoArgs::for_devid(devid: Devid)` performs the single `.get()`
  unwrap *internally* (`self.devid = devid.get()`), so the only raw `u64` devid
  left in the file is the `#[repr(C)]` ABI struct field itself (allowlist #1).
  Callers (`total_bytes`) pass `Devid` straight through -- no `.get()` at the call
  site, and verification #5's scalar grep therefore sees no `for_devid(devid: u64)`
  seam (the lone btrfs_ioctl hit is the allowlisted struct field). The test mock's
  `HashMap<(PathBuf, u64), u64>` key becomes `(PathBuf, Devid)` (value `u64` stays
  -- it is `total_bytes`).

### 10. Remaining production devid fields + natural flow
The inventory's journal / recover / remove / remove-missing / pool / status /
misc-renderer entries are **canonical field changes**, not incidental reads:
each is its own devid-bearing struct/enum/param that must become `Devid` (a
`u64` left here is a fresh count-accepting seam, the exact F1 hazard). Migrate
them per the inventory; the compiler surfaces every consumer once the fields
flip. Per-site work is almost all trivial:
- Formatting / preview / error strings (`lock.rs`, `repair_hint.rs`, recover
  previews, `OldDevidMismatch`): **no change** (`Display`).
- Comparisons against domain devids (`pool.missing_devids.contains(&devid)`,
  `d.devid == target_devid`): **no change** once both sides are `Devid` (`Eq`).
- `status.rs` devid maps: `HashMap<u64, String>` -> `HashMap<Devid, String>`,
  `HashSet<u64>` -> `HashSet<Devid>` (genuine safety gain -- wrong-id lookups now
  fail to compile).
- Any local `let x: u64 = ...devid` feeding a still-`u64` API: `.get()` (rare;
  the only standing one is the `btrfs_ioctl` ABI seam in step 9).

**Serialized devid surfaces** (all keep byte-identical shape via
`#[serde(transparent)]` value / `Display` key -- no fixture regen):
- `pool.json` -- `DiskMember.devid` (`"devid":1`).
- `alert-latch.json` -- `AlertCause` (`{"type":"missing_device","devid":7}`).
- `acked-stats.json` -- string keys via `devid.to_string()`.
- journal / pending-op JSON -- `OpKind`/`ReplaceJournalSource` `old_devid`/`devid`.
- status JSON -- `DiskReport.devid` and the `missing_devids` array.

### 11. Tests and fixtures
- Rust literals `devid: 5` / `vec![1, 2]` in `#[cfg(test)]` blocks and
  `cli/src/test_fixtures/*` (shared, status, remove_missing, monitor, ack,
  doctor, replace, remove) -> `Devid::new(5)` / `vec![Devid::new(1), ...]`. This
  is the bulk of the mechanical churn (~80-100 sites) and is unavoidable in any
  option that changes `PoolDevice.devid`.
- Parser assertions (`assert_eq!(out.devices[0].devid, 1)`) -> compare against
  `Devid::new(1)`.
- **Separate `cli/tests/` integration crate** (not `#[cfg(test)]`, its own
  compilation unit, so `cargo build` never surfaces it -- only `cargo test`):
  `tests/support/golden_common.rs` collects `out.devices.iter().map(|d| d.devid)`
  into a `Vec<u64>` and asserts `== vec![1, 2]`, plus per-device `assert_eq!(...
  .devid, 1)`. The `Vec<u64>` annotation -> `Vec<Devid>` and the literal vec ->
  `vec![Devid::new(1), Devid::new(2)]`. Easy to miss because it builds only under
  `cargo test`; M2's exit command (`just test-rust`) is what catches it.
- Test fixture loops that compute devid ints (`replace.rs#synth` `next_devid`)
  keep the `u64` counter and wrap at the `PoolDevice { devid: Devid::new(...) }`
  literal.
- **No `.json` / `.txt` fixture files change** (`cli/tests/fixtures/**`,
  `pool.json`/`alert-latch.json` test bytes): serde-transparent preserves them.

### 12. New behavioral tests (beyond mechanical literal updates)
Two new tests guard regressions the mechanical edits alone cannot catch.

- **Alert-carrier semantics (guards the swap fix).** Removing the positional
  params structurally prevents a *caller* swap, but a `AlertPoolState::alert_devids()`
  builder that fills `recognized`/`missing` backwards would still compile and
  invert behavior. Add a `probe.rs` (or `alert.rs`) test that builds an
  `AlertPoolState` with all three devid origins distinct -- a present devid, a
  btrfs-`MISSING` devid, and a null-underlying devid -- calls `alert_devids()`,
  then feeds the carrier through both `compute_alert_state` and `snapshot_current`
  and asserts: the missing/null-underlying devids latch `MissingDevice` and are
  marked `missing_acked`, while the present devid does **not** alert missing and
  **is** snapshotted. A backwards builder flips these assertions. (Extends the
  existing `probe_pool_alerts_*_method` tests, which today only check the loose
  vec helpers.)
- **`AlertState` JSON shape (guards latch format).** Same-type serde round-trips
  cannot catch a changed `AlertCause` shape. Add an **exact-string** assertion:
  serialize `AlertState { causes: vec![MissingDevice { devid: Devid::new(7) },
  BtrfsDeviceErrors { devid: Devid::new(2) }] }` and assert the JSON contains
  `{"type":"missing_device","devid":7}` and `{"type":"btrfs_device_errors","devid":2}`
  (bare integers, not `{"devid":{...}}`), plus a deserialize-back equality. This
  pins that `#[serde(transparent)]` keeps `alert-latch.json` byte-compatible and
  that the existing legacy-key test (`alert.rs` `{"type":"missing_device","devid":7}`)
  still parses.

## Seams where conversion is explicit (the only non-mechanical sites)
- **Construct** (`Devid::new`): all four parser conversion points (step 1 --
  `btrfs_filesystem_show` text build, `btrfs_device_stats` `.map`, `btrfs_device_usage`
  `.map`, `btrfs_scrub_status_per_device` `finalize`); the `--missing-id` clap arg;
  the JSON-key read-back in `alert.rs`.
- **Unwrap** (`.get()`): inside `btrfs_ioctl.rs#for_devid(devid: Devid)`, writing
  the raw `#[repr(C)]` `BtrfsIoctlDevInfoArgs.devid` field (kernel ABI) -- the only
  standing unwrap, and it lives *below* the `Devid`-typed `for_devid` signature, not
  at the call site. Argv and JSON-key *writes* use `Display`/`to_string()`, not
  `.get()`.

## Conventions to honor (`AGENTS.md`)
- `///` on `pub struct Devid` stating why it exists at the boundary (intent/
  invariant), per the doc-comment rule -- drafted above.
- CLI output stays ASCII; `Display` emits the bare number, no Unicode.
- Conventional commit, lowercase first line, e.g.
  `refactor(cli): wrap btrfs devid in a Devid newtype`.

## Verification
1. `cargo build` then `cargo clippy --all-targets` from `cli/` -- the migration is
   compiler-driven; a clean build means every devid hop typechecks and the
   devid-vs-count separation holds.
2. `just test-rust` (or `cargo test` in `cli/`) -- exercises alert/ack
   (`compute_alert_state`, `snapshot_current` via the new `AlertDevids`),
   `probe`, `replace` (`OldDevidMismatch`), `membership` (`by_devid`,
   `DuplicateDevid`), and the parsers.
3. `just test-parsers` -- confirms the `--json`/text parsers still deserialize the
   unchanged fixtures into the now-`Devid` fields.
4. Format-stability check -- one assertion **per serialized surface** (same-type
   round-trips alone can miss a changed inner shape, so each uses an exact-string
   or contains assertion, not just `from(to(x)) == x`):
   - `pool.json` -- `DiskMember` serializes `"devid":1`.
   - `alert-latch.json` -- the new `AlertState` JSON-shape test from step 12
     (`{"type":"missing_device","devid":7}` / `"btrfs_device_errors"`).
   - `acked-stats.json` -- string keys unchanged.
   - journal / pending-op JSON -- `old_devid`/`devid` emit bare integers.
   - status JSON -- `DiskReport.devid` and `missing_devids` array unchanged.
   Prefer asserting existing golden-JSON tests still pass byte-for-byte.
5. Residual-seam grep (proves the F1 inventory is clear) -- two passes, because
   devids hide under a different name and inside maps:
   - **Scalars/slices, incl. the `missing_id` alias:**
     `rg -n '\b(devid|missing_id|old_devid|target_devid)\b\s*:\s*(u64|Option<u64>|Vec<u64>|&\[u64\])' cli/src`
     should return **only** allowlist entries (kernel ABI `BtrfsIoctlDevInfoArgs`,
     parser-local raw accumulators, test-only synthesis, clap args in `main.rs`).
   - **Devid-bearing maps/sets:**
     `rg -n '(HashMap|BTreeMap|HashSet|BTreeSet)<[^>]*u64[^>]*>' cli/src` --
     eyeball each hit against the inventory's devid-map list (status
     `devid_to_name`, TUI `DiskIdentity.devid` + `tui/probe` name maps, membership
     dup-detection, monitor/alert `still_relevant`/`recognized`/`missing` sets);
     any map whose `u64` is a *devid* (not a count/size, allowlist #3) must be
     `Devid`-typed. Any other hit in either pass is an un-migrated production seam.
6. Swap-hazard spot check: confirm `compute_alert_state`/`snapshot_current` now
   take a single `&AlertDevids` (grep for the old two-slice signature returning
   nothing), and that the step-12 alert-carrier test passes -- the positional swap
   is structurally gone and a backwards builder is caught.
7. `just docs-build` if any doc/ADR references the alert function signatures.

## Implementation notes

- `AlertDevids` and its `alert_devids()` builder live in `cli/src/probe.rs`
  (the plan allowed `alert.rs` or `probe.rs`); `alert.rs` imports it.
- `matches!` patterns cannot contain function calls, so test assertions like
  `matches!(r, BtrfsFilesystemResize { devid: Devid::new(2), .. })` became the
  guard form `matches!(r, BtrfsFilesystemResize { devid, .. } if *devid ==
  Devid::new(2))` (recover.rs, replace.rs tests).
- The `DeviceUsageSpec` fixture family (`devid: u64` field, `live`/`missing`
  builders, the `*_usage_live_device` wrappers) and the stats-JSON string
  synthesizers (`source_io_stats_json`, `dirty_source_stats_handler`) keep raw
  `u64` devid params: they synthesize parser-input *text*, not Devid-bearing
  structs, so they fall under allowlist #4 rather than the named-helper rule.
- `target_validation_device` takes `Devid` per allowlist #4's named-helper
  rule but synthesizes a LuksUuid via `{raw:012x}`; `Devid` has no `LowerHex`,
  so the helper unwraps once internally with `.get()` (test-only; the
  production unwrap inventory is unchanged). `doctor_btrfs_show` does the same
  for its fixture-text formatting.
- `ack.rs`, `discover.rs`, and `progress.rs` use `Devid` only in their test
  modules, so the import lives in the `#[cfg(test)]` module rather than the
  production import list (avoids unused-import warnings on `cargo build`).
- The pre-existing `probe_pool_alerts_{alert_missing,recognized}_devids_method`
  tests cited the loose vec helpers that the `AlertDevids` carrier replaced;
  they were renamed (`probe_pool_alerts_alert_devids_missing`,
  `probe_pool_alerts_recognized_devids`) and their preambles now cite
  `alert_devids()`.
- ADR 014 (`docs/design/decisions/014-alerts.md`, Active) and
  `docs/internals/tool-behavior/device-disappearance.md` referenced the removed
  `recognized_devids`/`alert_missing_devids` helpers; both updated to cite the
  `AlertDevids` carrier per the AGENTS.md authority rule.
