# Plan: distinguish UUID-mismatch members in TUI + status

## Context

A declared pool member whose on-disk LUKS2 header carries a UUID that differs
from the UUID recorded in membership (the disk was reformatted, re-keyed, or
its slot re-imaged out-of-band) is the exact "swap / clone / reformat" case
decision 024 was built to surface (`docs/design/decisions/024-luks-uuid-identity.md:66-68`).

`braid doctor` already detects it (`DiskState::LuksUuidMismatch { expected, observed }`,
`doctor.rs:289-374`) and is the authoritative fail-closed gate. But the two
**at-a-glance read-only diagnostics silently diverge**: both the TUI disk table
(`tui/probe.rs:386-394`) and `braid status` (`status.rs:1036-1038`) discard the
observed UUID and collapse a mismatched member into the same generic "unknown"
cell as a foreign disk -- even though the recorded member UUID is in scope at
both sites. The operator's primary glance can't tell a reformatted member apart
from a stray disk.

This is a diagnostic-only correctness gap (not a safety bug -- `doctor` is
unaffected). The fix makes the **status detail + compact summary + TUI mismatch
decision** share one classifier so they can't drift, reusing doctor's existing
remediation wording.

**Scope of the "shared" guarantee (precise):** the classifier unifies *mismatch
detection* across status (both its sub-surfaces) and the TUI. It does **not**
fold in `doctor` -- doctor keeps its own inline `observed == *expected_uuid`
comparison (`doctor.rs:366`) because it runs standalone and re-probes the device;
importing that probe cost into the glance surfaces would be wrong. It also does
not force identical *rendering*: the correct-but-offline member (`Matches` but
not in the live pool) deliberately renders `Missing` in the TUI vs `Unknown` in
status, matching each surface's existing conventions (see the precedence table).

## Approach

Both glance sites already hold the observed UUID (from `probe_config_disk` ->
`ConfigDiskState::PresentLuks { uuid }`) and the recorded UUID (membership), so
neither re-probes.

**Shared classification policy** (one tested place, in `luks.rs` next to
`luks_uuid_mismatch_guidance()`):

```rust
/// Verdict for a present declared member whose on-disk LUKS UUID is compared
/// against the UUID recorded in membership. Shared by `status` and the TUI so
/// both glance surfaces agree on swap/reformat detection (decision 024);
/// previously only `doctor` compared the two and the read-only surfaces
/// silently diverged.
pub(crate) enum MemberLuksIdentity {
    /// On-disk UUID equals the recorded membership UUID.
    Matches,
    /// On-disk UUID contradicts the recorded UUID -- swapped, cloned, or
    /// reformatted out-of-band.
    Mismatch,
    /// No recorded UUID to compare against. Defensive: unreachable for
    /// declared members, which are UUID-keyed per decision 024.
    Unrecorded,
}

pub(crate) fn classify_member_luks_identity(
    observed: &LuksUuid,
    recorded: Option<&LuksUuid>,
) -> MemberLuksIdentity {
    match recorded {
        Some(expected) if expected == observed => MemberLuksIdentity::Matches,
        Some(_) => MemberLuksIdentity::Mismatch,
        None => MemberLuksIdentity::Unrecorded,
    }
}
```

**Per-surface mapping** (the live-pool guard stays surface-local -- it is the
TUI's existing defensive concern; status pre-filters by recorded UUID instead):

| Condition | TUI render | `status` detail + compact |
|---|---|---|
| observed in live pool, absent from `disk_usage` | `Missing` (existing) | n/a (pre-skipped by recorded-UUID filter) |
| `Matches` (our member, present, not assembled) | `Missing` | `Unknown` (status quo) |
| `Mismatch` | **`UuidMismatch`** (new, red) | **`LuksUuidMismatch`** (new) |
| `Unrecorded` | `UnknownLuks` (defensive fallback) | `Unknown` |

Per the locked decision, the correct-but-offline member (`Matches`) reuses the
existing `Missing` bucket in the TUI -- coherent with `probe.rs:388-390` ("treat
as Missing defensively rather than lying about state"). `UnknownLuks` shrinks to
the pure `Unrecorded` fallback.

**Compact/detail unification (status).** `braid status` (no flag) renders *both*
a compact `Drives:` summary and the detailed `Disks:` section
(`cmd_status:611-620` passes both unconditionally). Today `build_compact_drives`
(`status.rs:251-298`) classifies *every* unpooled member as `DiskStatus::Missing`
with no on-disk probe (`:293`), and the row renders `status` directly via its
`Display` (`:1250`). So the compact summary already shows `missing` for
present-but-unreadable/damaged members while the detail section refines them --
a pre-existing divergence. Adding `LuksUuidMismatch` to only the detail path
would *widen* it into an outright contradiction (`missing` vs `LUKS UUID
MISMATCH` for the same present disk in one invocation), and would leave the
mismatch unflagged on the literal primary glance.

Fix it structurally: compute the per-member unpooled status once and have both
sub-surfaces consume it.

- Reorder `build_status` so `build_disk_reports` (`:558`) runs before
  `build_compact_drives` (`:521`); ensure `config_disks` / `device_stats` are
  computed ahead of both.
- Build a `name -> DiskStatus` map from `verbose_ctx.disks` and pass it to
  `build_compact_drives`; in its unpooled loop, replace
  `status: DiskStatus::Missing` with
  `member_status.get(name).copied().unwrap_or(DiskStatus::Missing)`. Present rows
  (from `pool.devices`) and the devid logic are unchanged.
- Net effect: compact and detail can never contradict, and compact also gains
  accurate `unknown` / `luks-header-unreadable` / `luks-header-damaged` /
  `luks-uuid-mismatch` rows for unpooled members instead of a blanket `missing`
  (a strict accuracy gain). Genuinely absent members (not in the map) still
  render `missing`.

The TUI cell stays terse (`uuid mismatch`, red); the authoritative
expected-vs-observed pair lives in `braid doctor`. `status`'s human detail adds
the observed UUID plus the shared `luks::luks_uuid_mismatch_guidance()`
remediation line.

## Files to modify

### Shared classifier
- **`cli/src/luks.rs`** (near `luks_uuid_mismatch_guidance()` at :710) -- add
  `MemberLuksIdentity` + `classify_member_luks_identity`. `LuksUuid` already
  imported; no new deps.

### TUI (always)
- **`cli/src/tui/model.rs:240-263`** -- add unit variant
  `UnpooledDiskRender::UuidMismatch` with a `///` doc (decision 024; full
  expected/observed lives in `doctor`). Unit variant preserves
  `#[derive(..., Copy)]` -- `LuksUuid(String)` is not `Copy`, so a data-carrying
  variant would force dropping it. Refine the `UnknownLuks` doc to "valid LUKS
  header with no recorded membership UUID to compare against (defensive
  fallback)".
- **`cli/src/tui/view/mod.rs:820-840`** -- add the match arm in
  `unpooled_disk_status_cell`:
  `UnpooledDiskRender::UuidMismatch => Span::styled("uuid mismatch", Style::default().fg(Color::Red))`.
  (Only exhaustive match on the enum; compiler-enforced.)
- **`cli/src/tui/probe.rs:386-394`** -- replace the `else` with the classifier:
  ```rust
  ConfigDiskState::PresentLuks { uuid, .. } => {
      if live_pool_uuids.contains(&uuid) {
          UnpooledDiskRender::Missing
      } else {
          match luks::classify_member_luks_identity(&uuid, disks.luks_uuid.get(disk_name)) {
              luks::MemberLuksIdentity::Matches => UnpooledDiskRender::Missing,
              luks::MemberLuksIdentity::Mismatch => UnpooledDiskRender::UuidMismatch,
              luks::MemberLuksIdentity::Unrecorded => UnpooledDiskRender::UnknownLuks,
          }
      }
  }
  ```

### status -- detail path
- **`cli/src/status.rs:178-196`** -- add `DiskStatus::LuksUuidMismatch`; `Display`
  => `"luks-uuid-mismatch"`.
- **`cli/src/status.rs:1036-1062`** -- change `PresentLuks { .. }` to
  `PresentLuks { uuid, .. }`; classify against
  `membership.by_name(&cd.name).map(|(u, _)| u)` (`membership.rs:270` returns
  `Option<(&LuksUuid, &DiskMember)>`). Map `Mismatch -> LuksUuidMismatch`,
  `Matches | Unrecorded -> Unknown` (preserves today's behavior except the
  mismatch).
- **`cli/src/status.rs:1065-1083`** -- populate the unpooled report's `luks_uuid`
  with the observed UUID **only when classified `LuksUuidMismatch`**; leave it
  blank (`String::new()`) for the `Unknown` path, as today (per review Low #1 --
  do not add a `LUKS:` line to the offline-member row).
- **`cli/src/status.rs:1336-1413`** -- add the `LuksUuidMismatch` render arm and a
  remediation hint using `luks::luks_uuid_mismatch_guidance()`. The precise
  expected-vs-observed pairing stays authoritative in `braid doctor`.

### status -- compact path (unification)
- **`cli/src/status.rs:431-590` (`build_status`)** -- reorder so
  `build_disk_reports` precedes `build_compact_drives`; build a
  `name -> DiskStatus` map from the reports and pass it in.
- **`cli/src/status.rs:251-298` (`build_compact_drives`)** -- add the
  `&HashMap<String, DiskStatus>` parameter; unpooled rows take their status from
  it, defaulting to `Missing`. Present-row and devid logic unchanged.

### Docs
- **`docs/commands/status.md:158-166`** -- the compact unification makes the
  `Drives:` list render the same `DiskStatus` vocabulary as the detail section,
  so the table heading at `:158` ("Disk states in the detail view:") is now
  under-scoped. Broaden it to cover both renderings, e.g. "Disk states (compact
  `Drives:` list and detail view):". Add a row to the table (`:160-166`):
  `**LUKS UUID MISMATCH** | Device present but its LUKS header UUID differs from
  the recorded member -- swapped, cloned, or reformatted; run braid doctor`. The
  compact example at `:112-119` stays as illustrative `present`/`missing` output
  (a sample, not an exhaustive enumeration), so it needs no edit. `README.md`
  does not enumerate these states (cookbook style); `doctor.md` is unchanged
  (doctor behavior untouched).

## Tests

- **`cli/src/luks.rs`** -- unit test for `classify_member_luks_identity`
  (Matches / Mismatch / Unrecorded). The "tested once" anti-drift payoff.
- **`cli/src/tui/probe.rs`**:
  - **Update + rename** `unpooled_disk_present_luks_unknown_uuid_classified_as_unknown_luks`
    (:1973). It seeds observed `99999999...` vs recorded `22222222...` (ironwolf)
    -- the mismatch case -- so it must now expect `UnpooledDiskRender::UuidMismatch`;
    rewrite the Intent/Why/Scenario preamble for a reformatted/swapped member.
  - **Add**: observed == recorded (`22222222...`), not in live pool ->
    `UnpooledDiskRender::Missing` (pins the offline-member decision).
  - **Add**: `DiskIdentity` with `luks_uuid` omitting the disk -> `Unrecorded` ->
    `UnpooledDiskRender::UnknownLuks` (keeps the defensive fallback tested).
- **`cli/src/tui/view/mod.rs:2356`** -- in
  `unpooled_disk_status_cell_renders_each_variant`, add a `UuidMismatch` fixture;
  assert content `"uuid mismatch"` and `Color::Red`.
- **`cli/src/status.rs` -- detail classification (review High #1):**
  - **Update + re-preamble** `build_disk_reports_foreign_config_uuid_does_not_hide_missing_member`
    (:5002-5043). It seeds config `disk1` observed `99999999...` while membership
    records `disk1 -> 11111111...` (`status_membership_1disk`,
    `test_fixtures/status.rs:118-128`) and `11111111` is not live, so it reaches
    the classifier and the assertion at `:5042` must flip
    `DiskStatus::Unknown -> DiskStatus::LuksUuidMismatch`. Rewrite its preamble to
    describe the now-distinguished mismatch (mirroring the `probe.rs:1973` rename).
  - **Verified exclusions (callsite sweep, AGENTS.md):** the other unpooled
    `PresentLuks` constructions stay correct without edits because their members
    are UUID-matched and live, so the `:1029` recorded-UUID filter pre-skips them
    to `Present`: `build_disk_reports_sorts_present_rows_by_name_not_devid`
    (:5147/:5156), `build_disk_reports_routes_foreign_mapper_errors_to_doctor`
    (:5250, `member_uuid == 11111111`, live), `disk_report_pairs_stats_by_devid`
    (:6200). Re-run `rg -n "PresentLuks" cli/src/status.rs` as the verification.
- **`cli/src/status.rs` -- human render (review Medium):** add
  `status_verbose_luks_uuid_mismatch_disk`, mirroring
  `status_verbose_luks_header_unreadable_disk` (:2720) /
  `status_verbose_luks_header_damaged_disk` (:2784). Assert the `LUKS UUID
  MISMATCH` line and that the output contains the `luks_uuid_mismatch_guidance()`
  text (behavioral, structure-insensitive substrings).
- **`cli/src/status.rs` -- JSON token:** assert `"status" == "luks-uuid-mismatch"`
  alongside the existing token assertions at ~:1988.
- **`cli/src/status.rs` -- compact (review High #2):**
  - **Add** a `build_compact_drives` test: a `Mismatch` member renders
    `luks-uuid-mismatch` (not `missing`) in the compact row, and a genuinely
    absent member still renders `missing`.
  - **Update** the existing direct-call compact tests for the new parameter
    (pass an empty / all-`Missing` map; their absent members keep `Missing`, so
    assertions hold): `status_compact_missing_disk` (:5523),
    `status_compact_names_present_disk_from_membership_uuid` (:5540),
    `status_compact_foreign_mapper_name_does_not_hide_missing_member` (:5579),
    `build_compact_drives_sorts_present_rows_by_name_not_devid` (:5613),
    `build_compact_drives_missing_member_shows_devid_when_live_confirmed` (:5670),
    `build_compact_drives_missing_member_hides_stale_persisted_devid` (:5698).

## Verification

- `just test-rust` -- the primary gate: shared classifier, both probe/status
  classification flips, the human-render and compact tests, and the view render
  test. Pure Rust render/classification logic -- no tool-parser, systemd, or
  mutating-command surface.
- `rg -n "PresentLuks" cli/src/status.rs` and `rg -n "UnknownLuks" cli/src/tui/probe.rs`
  -- confirm the flipped tests are the only ones asserting the old verdict and
  the listed exclusions stay `Present`/`Missing`.
- `mdbook build docs` -- validates the `status.md` table edit (no new links; must
  not break the build).
- No VM tests required: read-only diagnostic rendering only, not
  mount/unlock/lock lifecycle, parsers, or mutating planning. A broader
  `just test-vm` pass is the operator's call, not this change's blast radius.
- Manual TUI/`status` confirmation isn't reproducible in-tree (needs a member
  with a reformatted header); the unit tests model it via mocked
  `cryptsetup luksUUID` output, as the existing unpooled-disk tests do.

## Implementation notes

- The plan's detail-test inventory ("Verified exclusions") did not name
  `build_disk_reports_foreign_mapper_name_does_not_hide_missing_member`
  (`status.rs`), which carries the *same* `assert_eq!(ctx.disks[1].status,
  DiskStatus::Unknown)` text as the flipped `..._foreign_config_uuid_...` test.
  It was left unchanged on purpose: it builds its config disk via
  `status_cfg_present_not_luks` (the `PresentNotLuks` arm, untouched by this
  change), so with the default `MockRunner` `probe_luks_header` returns
  `ProbeFailed` and the row stays `Unknown`. Only the `PresentLuks`-literal test
  reaches the new classifier. (`rg PresentLuks` doesn't surface the
  PresentNotLuks test, so the duplicate-string match only showed up at edit
  time.)
- Detail human-render action line: `LuksUuidMismatch` gets its own
  `else if` branch in `format_status_human` that emits
  `luks::luks_uuid_mismatch_guidance()` plus a "run 'braid doctor' for the
  expected vs observed UUID" pointer, rather than being folded into the generic
  `needs_doctor` ("run 'braid doctor' for recovery guidance") branch. The plan
  specified reusing the shared guidance but not the control-flow placement; a
  dedicated branch keeps the mismatch's distinct, more actionable wording while
  leaving the header-state branch untouched.
