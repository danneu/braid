# Surface LUKS header Unreadable/Damaged in status, unlock, and TUI

## Context

A previous change (commits `eaeec7c`, `155c5cf`) added `luks::LuksHeaderState`
and `luks::probe_luks_header` in `cli/src/luks.rs:241-275`, plus guidance
helpers (`luks_header_unreadable_guidance`, `luks_header_damaged_guidance`) at
`cli/src/luks.rs:284-299`. Both `braid doctor` and the on-failure
`explain_open_failure` helper in `cli/src/mount.rs:268-292` use the
distinction — Damaged gets `cryptsetup repair` guidance, Unreadable gets
off-system-backup guidance.

Three follow-ups were recorded and deferred:

1. `braid status` still routes `ConfigDiskState::PresentNotLuks` to the
   generic `DiskStatus::Unknown` bucket at `cli/src/status.rs:833-836`. Users
   reading `braid status` cannot tell "LUKS header unreadable" from other
   unknown states. The TUI has no notion of unpooled-disk state at all and
   renders every declared-but-not-in-pool disk by absence from the
   `disk_usage` map.
2. `cli/src/mount.rs::format_degraded_refused` (lines 56-78) is emitted
   *before* any unlock attempt. It lists missing/damaged disks with short
   labels and a `--allow-degraded` hint but does not point users at
   `braid doctor` for recovery guidance, which is where the full
   Unreadable/Damaged guidance already lives.
3. `ConfigDiskState::PresentNotLuks` at `cli/src/types.rs:121-128` is a unit
   variant. `probe_config_disk` runs `CryptsetupLuksUuid` once and collapses
   any exit-non-zero into that single case, so nothing downstream can tell
   Unreadable from Damaged from the read-only probe path.

Intended outcome: after this change, a disk with a damaged LUKS header is
reported as "LUKS header damaged" and a disk with no readable header is
reported as "LUKS header unreadable" consistently across `braid status`
(JSON + human text), the TUI disk table, and the pre-unlock degraded-refused
error. The common-case healthy unlock probe loop pays zero additional
cryptsetup cost.

## Design constraint: keep mutating commands on the coarse state

`add`, `replace`, and `enroll` currently treat `ConfigDiskState::PresentNotLuks`
as "fresh disk, will be LUKS-formatted" and route it through the destructive
format/wipe path. A LUKS header that is `Damaged` (isLuks ok, luksDump
failed) is potentially **recoverable** via `cryptsetup repair` and must
never be silently re-formatted. Therefore the Unreadable/Damaged refinement
must NOT propagate into mutating-command code paths — those paths must keep
seeing the coarse `PresentNotLuks` state and continue to confirm/format as
they do today.

Concretely: this plan does **not** split `ConfigDiskState::PresentNotLuks`
into multiple variants and does **not** modify `probe_config_disk`,
`cli/src/types.rs`, `add.rs`, `replace.rs`, or `enroll_key_file.rs`. The
refinement happens only inside the three diagnostic call sites that need it
(`status.rs`, `mount.rs::plan_open_pool`, `tui/probe.rs`), each calling
`luks::probe_luks_header` directly on the already-broken `PresentNotLuks`
branch.

## Critical files

- `cli/src/luks.rs` — already provides `LuksHeaderState` + `probe_luks_header` (lines 241-275); reuse, do not duplicate.
- `cli/src/status.rs` — `DiskStatus` enum (lines 142-149), `Display` impl (lines 151-160), unpooled config disk loop (lines 821-861), human-text rendering (lines 1031-1092), existing JSON/text tests (lines 1724-2296).
- `cli/src/mount.rs` — `MissingReason` + `format_degraded_refused` (lines 42-78), `plan_open_pool` `PresentNotLuks` arm (lines 128-131), existing tests (lines 888-1043).
- `cli/src/tui/model.rs` — `PoolState` struct (lines 67-82).
- `cli/src/tui/probe.rs` — live-pool probe (lines 17-107).
- `cli/src/tui/view/mod.rs` — disk table rendering (lines 330-451, particularly 360-382).

Untouched on purpose: `cli/src/probe.rs`, `cli/src/types.rs`, `cli/src/add.rs`,
`cli/src/replace.rs`, `cli/src/enroll_key_file.rs`.

## Part A — `braid status`: refine the unpooled bucket

Add two variants to `cli/src/status.rs:142-149`:

```rust
pub enum DiskStatus {
    Present,
    Missing,
    LuksHeaderUnreadable,
    LuksHeaderDamaged,
    Unknown,
    New,
}
```

- Extend the `Display` impl at lines 151-160. The enum has
  `#[serde(rename_all = "lowercase")]`. Confirm serde's behavior on the
  multi-word variants by running the existing JSON tests (lines 1724-1798)
  after the addition: serde renders `LuksHeaderUnreadable` as
  `"luksheaderunreadable"` under `lowercase`. If kebab-case is preferred for
  readability, switch the enum-level rename to
  `#[serde(rename_all = "kebab-case")]` and update the existing single-word
  test expectations (`"present"` etc. continue to work because kebab-case
  collapses to the same single-word string).

- Update the unpooled config disk loop at `cli/src/status.rs:833-836`. The
  loop currently has access to `runner` (it is inside `build_disk_reports`
  which already takes the runner). Refine the `PresentNotLuks` arm:

  ```rust
  let status = match &cd.state {
      ConfigDiskState::Absent => DiskStatus::Missing,
      ConfigDiskState::PresentLuks { .. } => DiskStatus::Unknown,
      ConfigDiskState::PresentNotLuks => match luks::probe_luks_header(runner, &cd.by_id_path.0) {
          luks::LuksHeaderState::Unreadable => DiskStatus::LuksHeaderUnreadable,
          luks::LuksHeaderState::Damaged => DiskStatus::LuksHeaderDamaged,
          // luksUuid failed but luksDump succeeded — treat as damaged
          // (less destructive recovery suggestion).
          luks::LuksHeaderState::Ok => DiskStatus::LuksHeaderDamaged,
          // The probe itself could not run; we already know the header is
          // not yielding a UUID, so collapse to the generic Unknown bucket
          // rather than guessing.
          luks::LuksHeaderState::ProbeFailed(_) => DiskStatus::Unknown,
      },
  };
  ```

  The extra cryptsetup invocations land only on disks that already failed
  `luksUuid`, so the healthy-pool case stays at one cryptsetup call per
  disk.

- Convert the human-text rendering at `cli/src/status.rs:1031-1092` from the
  chain of `if d.status == ...` comparisons to a single `match` on
  `d.status`. Add arms for the two new variants printing
  `"LUKS HEADER UNREADABLE"` / `"LUKS HEADER DAMAGED"` under the disk name
  in the same column position the existing `MISSING`/`NEW`/`UNKNOWN` labels
  use.

- Update the "errors" conditional block at lines 1075-1080 so the new
  states print `"unknown (LUKS header unreadable)"` /
  `"unknown (LUKS header damaged)"` instead of falling through to the
  generic `"unknown (metadata unavailable)"` line.

- Update the action-guidance conditional at lines 1087-1092: both new states
  should trigger the action-guidance hint, pointing the user at
  `braid doctor`.

### Tests to add in `cli/src/status.rs`

- Extend `status_json_verbose_disks` (around line 1724) with two additional
  `DiskReport` entries — one for each new variant — and assert the JSON
  serializes the expected status string.
- Add a `format_status_human_*` test that builds a `HumanDisk` for each new
  `DiskStatus` variant and asserts the rendered text contains the expected
  header-status label and the doctor action-guidance line.

The new probe-time call to `luks::probe_luks_header` is exercised at the
status integration level via the existing fake-runner test scaffolding —
add per-status fixtures wired into the `build_disk_reports`-level test that
already covers `ConfigDisk` shapes (search for tests in `status.rs` that
construct `ConfigDisk` values with `ConfigDiskState::PresentNotLuks` and
clone the pattern, returning the cryptsetup output sequences for each new
case).

## Part B — `format_degraded_refused`: split label + doctor footer

Extend `cli/src/mount.rs:42-48`:

```rust
enum MissingReason {
    Unplugged,
    LuksHeaderUnreadable,
    LuksHeaderDamaged,
}
```

In `plan_open_pool` (`cli/src/mount.rs:121-151`), the `PresentNotLuks` arm
currently always pushes `MissingReason::LuksHeaderUnreadable`. Refine it
in-place by calling `luks::probe_luks_header` and selecting the right
`MissingReason`:

```rust
ConfigDiskState::PresentNotLuks => {
    let reason = match luks::probe_luks_header(runner, &member.by_id.0) {
        luks::LuksHeaderState::Damaged => MissingReason::LuksHeaderDamaged,
        // Unreadable, Ok-but-luksUuid-failed, and ProbeFailed all fall back
        // to the existing Unreadable label — Damaged is the only refinement
        // we promote out of this branch, because Damaged is the only state
        // that has a distinct `cryptsetup repair` story.
        _ => MissingReason::LuksHeaderUnreadable,
    };
    eprintln!(
        "{}  disk: {:<10}{}",
        tag("skip"),
        name,
        match reason {
            MissingReason::LuksHeaderDamaged => "LUKS header metadata damaged",
            _ => "LUKS header unreadable",
        }
    );
    missing.push((name.clone(), reason));
}
```

(The fallback collapse keeps the diagnostic conservative: if `probe_luks_header`
cannot prove the header is metadata-damaged, the original "unreadable" label
stays.)

In `format_degraded_refused` (`cli/src/mount.rs:56-78`):

1. Add a `MissingReason::LuksHeaderDamaged => "LUKS header metadata damaged"`
   arm to the existing match.
2. After the existing `"hint: braid <cmd> --allow-degraded"` line, append a
   second line conditionally: if any disk in `missing` has
   `MissingReason::LuksHeaderUnreadable` or
   `MissingReason::LuksHeaderDamaged`, append
   `"run 'braid doctor' for recovery guidance"`. Do not emit the footer in
   the Unplugged-only case — a simple unplugged-cable failure does not need
   doctor guidance.

### Tests to add in `cli/src/mount.rs`

- `format_degraded_refused_damaged_includes_disk_name_and_reason` — single
  `LuksHeaderDamaged` disk; assert output contains the disk name and
  `"LUKS header metadata damaged"`.
- `format_degraded_refused_unreadable_includes_doctor_footer` — single
  `LuksHeaderUnreadable` disk; assert output contains `"braid doctor"`.
- `format_degraded_refused_damaged_includes_doctor_footer` — single
  `LuksHeaderDamaged` disk; assert footer present.
- `format_degraded_refused_unplugged_only_omits_doctor_footer` — single
  `Unplugged` disk; assert footer absent.
- `format_degraded_refused_mixed_includes_doctor_footer_once` —
  `Unplugged` + `LuksHeaderDamaged`; assert both short labels present and
  footer appears exactly once.

Existing
`format_degraded_refused_does_not_reference_local_header_backups`
(line 1042) must continue to pass — the new footer references
`braid doctor`, not `/var/lib/braid/luks-headers/`.

## Part C — TUI: surface per-disk Unreadable/Damaged AND PresentLuks-not-in-pool

The TUI currently probes only the live mounted pool in `cli/src/tui/probe.rs`
(lines 17-107). Declared-but-unpooled disks have no representation in
`PoolState` and the disk table view at `cli/src/tui/view/mod.rs:360-382`
infers "missing" by absence from `disk_usage`.

Mirror `braid status`'s unpooled bucket. The TUI needs to handle THREE
distinct states for a declared disk that is not in the live pool:

1. `ConfigDiskState::Absent` → "missing" (matches today's behavior).
2. `ConfigDiskState::PresentLuks { uuid, .. }` where `uuid` is not in the
   live pool → "unknown" (LUKS header valid but not part of this pool).
   This case is in `braid status` today and the TUI must not collapse it
   into "missing".
3. `ConfigDiskState::PresentNotLuks` refined via `probe_luks_header` →
   "header unreadable" or "header damaged".

### Add a derived enum for what the TUI needs to render

```rust
// cli/src/tui/model.rs
#[derive(Clone, Debug)]
pub enum UnpooledDiskRender {
    Missing,
    UnknownLuks,                // PresentLuks { uuid not in pool }
    LuksHeaderUnreadable,       // PresentNotLuks → LuksHeaderState::Unreadable (or fallback)
    LuksHeaderDamaged,          // PresentNotLuks → LuksHeaderState::Damaged
}
```

The variant names are deliberately prefixed with `LuksHeader` (not just
`Header`) so the meaning is unambiguous in the view layer — `Header` alone
could read as a btrfs header, an lsblk column header, etc.

This is a TUI-local enum; it deliberately does not leak `LuksHeaderState`
or `ConfigDiskState` into the view layer.

### Add a field to `PoolState`

```rust
// cli/src/tui/model.rs (struct at lines 67-82)
pub unpooled_disks: HashMap<String, UnpooledDiskRender>,
```

### Populate during refresh

In `cli/src/tui/probe.rs::probe_pool_for_tui`, the existing call at line 23
already returns `let domain = probe_pool(...)`, where `domain.devices` is a
`Vec<PoolDevice>` and `PoolDevice` carries `luks_uuid: LuksUuid` (see
`cli/src/types.rs:96-101`). This is the authoritative live-pool UUID set —
build it explicitly:

```rust
use std::collections::HashSet;
use crate::types::LuksUuid;

let live_pool_uuids: HashSet<LuksUuid> = domain
    .devices
    .iter()
    .map(|d| d.luks_uuid.clone())
    .collect();
```

Then iterate the declared `disk_by_id` map (already passed into
`probe_pool_for_tui` at line 20) and, for each disk that is NOT already in
`disk_usage` (the live-pool data we just built), run
`probe::probe_config_disk` and classify into `UnpooledDiskRender`:

- `ConfigDiskState::Absent` → `UnpooledDiskRender::Missing`
- `ConfigDiskState::PresentLuks { uuid, .. }` →
  - If `live_pool_uuids.contains(&uuid)` → defensive `Missing`
    (the disk is in the pool by UUID but somehow not in `disk_usage`;
    this shouldn't happen but don't lie about the state).
  - Otherwise → `UnknownLuks` (a valid LUKS header whose UUID does not
    belong to this pool — exact mirror of `braid status`'s `Unknown`
    bucket).
- `ConfigDiskState::PresentNotLuks` → call
  `luks::probe_luks_header(runner, &by_id_path)` and map:
  - `LuksHeaderState::Damaged` → `LuksHeaderDamaged`
  - `LuksHeaderState::Unreadable` → `LuksHeaderUnreadable`
  - `LuksHeaderState::Ok` → `LuksHeaderDamaged` (consistent with status.rs)
  - `LuksHeaderState::ProbeFailed(_)` → `LuksHeaderUnreadable` (conservative;
    we already know the header is not yielding a UUID)

The `live_pool_uuids` set is the single source of truth for "is this disk
already counted as part of the live pool?". Do **not** use `luks_info`
(which carries cipher/keyslot metadata, not UUIDs) and do not use
`disk_usage` keys for the LUKS-membership question. `disk_usage` is fine
for the prior "is there live device data for this disk?" check, but it is
keyed by disk *name*, not LUKS UUID, so it cannot answer the
"valid LUKS but not in this pool" question on its own.

On `ProbeError` from `probe_config_disk` itself, do not fail the whole
refresh. Skip the disk: omit it from `unpooled_disks` so the view layer
falls back to its existing "declared but no probe data" rendering. The
existing TUI probe code uses `.ok()` / `.unwrap_or(...)` patterns for
optional probes (see `smart_health` at lines 79-88 and `disk_transport` at
108-120) — match that style for consistency.

### Render in the disk table

In `cli/src/tui/view/mod.rs:360-382`, factor the per-row status cell into a
small pure helper if it is not already factored:

```rust
fn disk_status_cell(state: &PoolState, name: &str) -> CellText {
    if let Some(usage) = state.disk_usage.get(name) {
        // existing live-disk path
    } else if let Some(render) = state.unpooled_disks.get(name) {
        match render {
            UnpooledDiskRender::Missing => /* existing "missing" styling */,
            UnpooledDiskRender::UnknownLuks => "unknown" /* dim yellow */,
            UnpooledDiskRender::LuksHeaderUnreadable => "LUKS header unreadable" /* red */,
            UnpooledDiskRender::LuksHeaderDamaged => "LUKS header damaged" /* red */,
        }
    } else {
        // existing fall-through (declared but no probe data — keep current behavior)
    }
}
```

Cell labels say `"LUKS header unreadable"` / `"LUKS header damaged"` in
full — never abbreviate to `"header ..."` in the TUI, because the disk
table already has columns whose names use the word "header" in other
contexts.

Match the colors and dim/bold attributes used by the existing "missing"
styling so the new states feel consistent visually.

### TUI test

Add one focused unit test against `disk_status_cell`: build a fake
`PoolState` with one entry per `UnpooledDiskRender` variant in
`unpooled_disks` and assert the rendered cell text. If the existing view
code does not factor the status cell into a helper, the refactor to extract
one is part of this plan and the test pins the helper's contract.

## Verification

Run from the braid repo root in this order:

1. `just test-rust` — exercises new `format_degraded_refused` tests, the
   `status.rs` JSON/human tests, and the TUI view helper test. Must pass.

2. `just test-parsers` — parser canary; no parser changes expected, but
   confirms the live-tool path is healthy.

3. `just test-vm` — focus on tests that exercise missing/unplugged disks
   and the degraded-refused path. Existing tests must pass unchanged
   (this plan does not change `add`/`replace`/`enroll` semantics, so the
   risk surface is contained).

4. **Manual TUI check**: in a test VM with a declared-but-damaged disk,
   open the TUI and confirm the disk table shows
   "LUKS header unreadable" / "LUKS header damaged" / "unknown" / "missing"
   correctly for each unpooled state.

5. **New VM test (recommended)**: stage a disk with a deliberately
   corrupted LUKS header (e.g. zero out the LUKS magic for Unreadable, or
   corrupt the JSON metadata block for Damaged), run `braid status --json`,
   and assert the disk's `status` field equals the new values. Add a
   second assertion that running `braid unlock` produces a degraded-refused
   error containing both the new short label AND the
   `"run 'braid doctor' for recovery guidance"` footer. This protects the
   end-to-end propagation against regression.

## Out of scope

- No changes to `ConfigDiskState`, `probe_config_disk`, `add.rs`,
  `replace.rs`, or `enroll_key_file.rs`. Mutating commands keep the coarse
  `PresentNotLuks` state and the destructive-format guards stay intact.
- No changes to `explain_open_failure` in `cli/src/mount.rs:268-292`; it
  already inlines the full guidance on mid-unlock open failures.
- No changes to `doctor.rs`; doctor's Unreadable/Damaged rendering is
  already correct.
- No changes to the `luks_header_unreadable_guidance` /
  `luks_header_damaged_guidance` wording or to the invariant that forbids
  referencing local `/var/lib/braid/luks-headers/` paths in user-facing
  recovery messages.
- No changes to `reference/` snapshots or fixture files; all new probe
  paths use existing `CryptsetupIsLuks` / `CryptsetupLuksDumpText`
  commands already supported by the runner.
