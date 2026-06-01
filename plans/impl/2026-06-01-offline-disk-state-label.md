# Unified `offline` disk-state label

## Context

A recorded pool member that is **physically present** and **LUKS-identity-verified**
(its on-disk LUKS UUID matches the `pool.json` membership UUID) but is **not in the
live btrfs array** is rendered inconsistently and misleadingly today:

- `braid status` -> `DiskStatus::Unknown` -> `UNKNOWN` / `"unknown"`
- TUI -> `UnpooledDiskRender::Missing` -> `missing` (yellow)

Both labels are wrong for this state. `missing` implies the disk is gone (it is
present -- the header was just read and matched). `unknown` implies braid could not
classify it (it did -- identity is verified). The two surfaces also disagree, even
though they already share the classifier `luks::classify_member_luks_identity`
(`cli/src/luks.rs:750`). The original drift was that each surface independently mapped
the same `MemberLuksIdentity::Matches` verdict to a different render bucket.

This state is real and reachable, not theoretical:

- **Degraded / locked member** (the everyday case): a member's mapper is closed
  while the pool is mounted degraded. `cryptsetup close braid-X` + `mount -o degraded`
  produces it deterministically -- this is exactly what `tests/cli/braid-status-rust.py:152-185`
  does to `disk3`.
- **`remove` post-commit window**: `remove.rs` runs btrfs-remove -> close mapper ->
  `save_membership` (the prune) **last** (`cli/src/remove.rs:474`). If the final persist
  fails (it has a dedicated `RemoveError::MembershipPersistFailure`), the disk is gone
  from the array but still recorded, present, header-matching.
- **Interrupted add/replace/remove** generally, until `recover` reconciles.

Precondition for both surfaces: the pool is mounted (status early-returns
`not_mounted_status` otherwise) and the member is absent from the live `disk_usage` /
`pool.devices`. Whether the LUKS mapper is open is irrelevant -- a present device with
a readable header classifies as `ConfigDiskState::PresentLuks` regardless
(`cli/src/probe.rs:813` `probe_config_disk_present_luks_closed`).

**Outcome:** add an `offline` state to both surfaces, mapped from
`MemberLuksIdentity::Matches`, with wording that is honest on both. Cause-neutral
(braid cannot tell locked-vs-half-removed from the on-disk state, and must not
overclaim a remedy). Render-only change: pool-level `StatusCode` is derived solely
from `pool.missing_count` (`cli/src/status.rs:527-530`), so the summary verdict cannot
shift.

## Settled decisions

- **Word:** `offline` (ZFS/RAID precedent for a known-but-not-participating member;
  cause-neutral). `OFFLINE` in verbose human status, `offline` in compact + JSON +
  TUI cell.
- **No `Action:` hint** for offline rows. The remedy is cause-ambiguous, and
  `braid doctor` reports an offline member as healthy (`declared_disks` classifies
  present + header-readable + UUID-match as `DiskState::LuksHeaderOk`,
  `cli/src/doctor.rs:361-362`), so there is no command to usefully point at. Matches
  the non-prescriptive `Unknown` precedent (`status.rs:5843`).
- **`luks_uuid` stays `""`** on the offline row, consistent with every non-mismatch
  unpooled row (only `Mismatch` surfaces the observed UUID -- `status.rs:1059-1060`).
- **Anti-drift = two variants + a per-surface test + cross-referencing comments.**
  No shared intermediate type: the two enums legitimately differ (serde+`Display`
  `DiskStatus` vs colored `UnpooledDiskRender` with `WrongLuksVersion`/`MapperHijacked`
  payloads that have no `DiskStatus` equivalent). The shared `classify_member_luks_identity`
  + a test on each arm is the guarantee.
- **Rationale home = extend decision 024** (it owns the classifier and the
  cross-surface-consistency invariant). No new ADR.

## Implementation

### 1. Types

- `cli/src/status.rs:178-184` -- add `Offline` to `DiskStatus` (after `LuksUuidMismatch`,
  before `Unknown`). `#[serde(rename_all = "kebab-case")]` already yields `"offline"`.
  Add a `///` doc comment capturing the invariant: present, LUKS identity matches the
  recorded membership UUID, not in the live array; distinct from `Missing`/`Unknown`;
  cause-neutral.
- `cli/src/status.rs:186-195` -- add `Self::Offline => f.write_str("offline")` to the
  `Display` impl. (This alone covers the compact `Drives:` list, which renders
  `d.status` via `{}` at `status.rs:1293` -- no separate compact match exists.)
- `cli/src/tui/model.rs:261-285` -- add `Offline` to `UnpooledDiskRender` (after
  `Missing`, before `UnknownLuks`), with a `///` doc comment in the existing
  variant-comment style (note the `ConfigDiskState::PresentLuks` + `Matches`
  precondition and why it is distinct from `Missing`/`UnknownLuks`).

### 2. Classification (map `Matches` -> offline; keep `Unrecorded` -> unknown)

- `cli/src/status.rs:1079-1081` -- split the combined arm:
  - `MemberLuksIdentity::Matches => (DiskStatus::Offline, String::new())`
  - `MemberLuksIdentity::Unrecorded => (DiskStatus::Unknown, String::new())` (defensive;
    declared members are UUID-keyed, so unreachable).
  - `Mismatch` arm (1073-1075) and the `PresentNotLuks` arm (1084-1105) unchanged.
  - Add a comment noting the TUI maps the same verdict to `UnpooledDiskRender::Offline`.
- `cli/src/tui/probe.rs:411` -- `MemberLuksIdentity::Matches => UnpooledDiskRender::Offline`
  (was `Missing`). The `Absent => Missing` (395) and `live_pool_uuids.contains` defensive
  `Missing` (397-401) arms stay. Add a comment noting `status` maps the same verdict to
  `DiskStatus::Offline`.

### 3. Render

- `cli/src/status.rs:1385-1387` -- verbose header arm:
  `DiskStatus::Offline => out.push_str(&format!("  {:<18}OFFLINE\n", d.name))`.
- `cli/src/status.rs:1441-1444` -- errors-line arm (before the `Unknown` arm):
  `None if d.status == DiskStatus::Offline => { out.push_str("    Errors:  unknown (disk offline -- not in pool)\n"); false }`.
- `cli/src/status.rs:1448-1470` -- **no change** (offline falls through to no `Action:`
  line, like `Unknown`).
- `cli/src/tui/view/mod.rs:821-839` -- cell arm, **yellow** (soft/recoverable, like the
  `missing` it replaces):
  `UnpooledDiskRender::Offline => Span::styled("offline", Style::default().fg(Color::Yellow))`.

### 4. Tests

- `cli/src/tui/probe.rs:2168` -- rename `..._classified_as_missing` ->
  `..._classified_as_offline`; flip assertion to `UnpooledDiskRender::Offline`. **Fix
  the stale comment** that cites a "locked offline-member decision (plan precedence
  table)" (no such committed doc) to cite decision 024's new subsection.
- `cli/src/tui/view/mod.rs:2355-2392` (`unpooled_disk_status_cell_renders_each_variant`)
  -- add `("delta".to_owned(), UnpooledDiskRender::Offline)` (`delta` is unused; avoid
  `hotel`, which the `is_none()` negative check uses), add `assert_eq!(cell("delta"), "offline")`,
  and add the first **yellow** color assertion (mirroring the existing red checks for
  `foxtrot`/`golf`).
- **New** `cli/src/status.rs` test `build_disk_reports_present_luks_matching_uuid_offline_classified_as_offline`
  -- adapt the mismatch test (`status.rs:5053`) with on-disk UUID **equal** to the
  recorded membership UUID and the pool **not** containing it; assert
  `status == DiskStatus::Offline` and `luks_uuid == ""`. Add the intent/why/scenario
  preamble. (No existing status test covers the `PresentLuks + Matches` path.)
- **New (mandatory)** verbose-render test beside `status_verbose_unknown_disk`
  (`status.rs:5843`): render a single `HumanDisk { status: DiskStatus::Offline, .. }`
  via `format_status_human(&report, None, Some(&human_disks), None)` and assert the
  output (a) contains `OFFLINE`, (b) contains `disk offline -- not in pool`, and
  (c) does **not** contain `Action:`. The `!contains("Action:")` assertion is the
  load-bearing one: every recovery hint in the verbose render (`status.rs:1448-1470`
  -- replace, mismatch guidance, doctor) is emitted as an `Action:` line, so it pins
  the deliberate no-hint decision against a future regression that adds a `braid doctor`
  / `replace` / other cause-ambiguous prompt. Also mirror the `!contains("braid doctor")`
  assertion the `Unknown` test already uses (`status.rs:5888-5891`). This protects a
  behavioral decision in the plan, so it is **not** optional.
- `tests/cli/braid-status-rust.py:152-185` (canonical e2e -- `cryptsetup close braid-disk3`
  + degraded mount): flip line 168 `"UNKNOWN"` -> `"OFFLINE"`; line 170
  `"metadata unavailable"` -> `"disk offline -- not in pool"`; lines 182-185 rename
  `unknown_disks` -> `offline_disks` and `status == "unknown"` -> `"offline"`. Leave the
  `DEGRADED` / `1 missing device` summary asserts (163-166) -- they read the pool-level
  summary, not the disk3 detail row.

### 5. Docs

- `docs/design/decisions/024-luks-uuid-identity.md` -- add a short `## Offline disk state`
  subsection (before "Limits And Non-Goals"): a present, identity-verified member absent
  from the live array renders `offline` on both surfaces, distinct from `missing`/`unknown`,
  cause-neutral, no prescribed remedy. Add two bullets to "Tests That Enforce This"
  (the new status + probe tests, and the `braid-status-rust.py` e2e).
- `docs/commands/status.md:160-166` -- add an `OFFLINE` row to the state table (after
  `MISSING`).
- `docs/commands/status.md` JSON `disks` field section (252-305) -- revise the **whole**
  section so the field docs distinguish **live-pool-member** rows from
  **diagnostic/unpooled** rows and stop tying blank/null fields to physical absence.
  `offline` is the row that breaks the current "non-present" framing: it is **present
  but not a live member**, yet carries the unpooled blank/null shape. Specific edits:
  - intro (252-258): add `offline` to the "configured disks that are not currently live
    pool members" list (currently `missing` / `unknown` / `luks-header-*`; while there,
    `luks-uuid-mismatch` is also omitted -- add it for completeness).
  - `luks_uuid` bullet (259-266): today the blank value is justified by "the UUID is read
    from the live device and is **unavailable when the device is absent**" -- false for
    `offline`, which is present and whose UUID *was* observed and matched, just not
    surfaced. Reframe as a three-way contract, **preserving the existing mismatch
    exception** (status surfaces the observed UUID for mismatch rows -- `status.rs:1073`,
    pinned by the JSON test at `status.rs:2040-2044`): (1) **live pool member** rows carry
    the live UUID (membership key for matched members, foreign live UUID for foreign
    devices); (2) **`luks-uuid-mismatch`** rows carry the **observed** UUID (the diagnostic
    exception); (3) other non-live rows (`missing`, `offline`, `unknown`,
    `luks-header-unreadable`) report `""` and are correlated by `name`. Do not tie the
    blank to device absence, and do not collapse the mismatch exception into the blank group.
  - `underlying` bullet (278-279): change "`null` when the disk is **not present**" ->
    "`null` when the disk is **not a live pool member**" (matching the already-correct
    `devid` bullet), since `offline` is present but has `underlying: null`.
  - `devid` bullet (280-281): already says "not a live pool member" -- no change.
  - `status` enum (282-283): add `offline`.
  - note (303-305): add `offline` to the row list that reports blank-`luks_uuid` /
    null-`devid` / null-`underlying` / no-`errors` (currently `missing` / `unknown` /
    `luks-header-*`), and name it as **present but not assembled** -- it joins that group
    for a *different* reason than the truly-absent rows (not a live member, vs. not
    present). Keep `luks-uuid-mismatch` out of this group (it carries the observed UUID).

## Out of scope (noted follow-up)

`braid doctor`'s `declared_disks` check reports an offline member as healthy
(`LuksHeaderOk`); it never cross-checks declared members against live-pool membership.
That is a latent doctor blind spot sharing this root cause, but it is a distinct change
to doctor's check logic -- not part of this label change.

## Critical files

- `cli/src/status.rs` (enum, Display, classification, verbose render, new test)
- `cli/src/tui/model.rs` (enum variant + doc comment)
- `cli/src/tui/probe.rs` (classification arm + test rename/flip + comment fix)
- `cli/src/tui/view/mod.rs` (cell arm + snapshot test)
- `docs/design/decisions/024-luks-uuid-identity.md`, `docs/commands/status.md`
- `tests/cli/braid-status-rust.py`

## Reuse (do not reinvent)

- `luks::classify_member_luks_identity` / `MemberLuksIdentity` (`cli/src/luks.rs:736-759`)
  -- already shared; both surfaces keep routing through it. No change.
- `Display for DiskStatus` (`status.rs:186`) -- the single source for compact rendering.

## Verification

Not a parser-critical change (no `pool.json` / `pending-op.json` / argv fixtures change),
so **no fixture refresh**.

1. `just test-rust` -- new + renamed unit tests, Display/serde, snapshot test.
2. `just test-vm braid-status-rust` -- canonical e2e (flake check `braid-status-rust`,
   `flake.nix:281`); confirm the degraded phase now shows `OFFLINE` and the
   `DEGRADED` / `1 missing device` summary asserts still pass.
3. `mdbook build docs` -- linkcheck (the 024 subsection adds no cross-links, but confirm).

## Follow Up

- `cli/src/doctor.rs`: teach `declared_disks` to cross-check declared members against live-pool membership so present, LUKS-identity-verified but unassembled members are not reported as healthy.
