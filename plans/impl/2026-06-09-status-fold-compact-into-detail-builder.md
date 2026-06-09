# Plan: classify each status disk once -- fold the compact summary into the detail builder

## Context

`braid status` builds two parallel per-disk views in `cli/src/status.rs`:

- **Detail** -- `build_disk_reports` classifies each disk once and emits a
  `DiskReport` (machine/JSON) + `HumanDisk` (verbose human detail) per row,
  returned in `VerboseContext`.
- **Compact** -- `build_compact_drives` is a *second* pass that produces the
  always-on "Drives:" summary (`Vec<CompactDrive>`).

`build_status` runs the detail pass, then derives a `member_status:
HashMap<String, DiskStatus>` bridge map from the detail reports and feeds it to
`build_compact_drives` so the compact verdict mirrors the detail verdict
(decision 024 swap/reformat detection). The cost: the present-device pass
(LUKS-UUID set + `present_display_name` join + sort-by-name) and the
unpooled-member iteration are duplicated across the two builders, kept in sync
only by the bridge map -- a divergence hazard the code already pays to guard
(the bridge itself, plus `build_compact_drives_unpooled_member_mirrors_detail_status`
and the `status_compact_*` tests that exist solely to police it).

**Goal:** classify each disk exactly once and project the verdict into all
three shapes, deleting the duplicated pass and the bridge map.

**Why the fold, not a projection.** An earlier read of this issue suggested
deriving the compact list as a standalone projection over the returned
`DiskReport`s. That is the wrong shape: the unpooled compact devid is
`member.devid.filter(|d| alert_devids.contains(d))`, and `DiskReport.devid` is
`None` for unpooled rows, so a projection must re-join membership to recover it.
`PoolMembership::by_name` takes `&DiskName` (no `&str` form) and
`DiskName::parse` is fallible -- so a projection over `report.name: String`
would force either an `expect` (a latent panic, forbidden in the
always-available status diagnostic) or a silent `.ok()` devid drop. Folding the
compact emission into the detail builder sidesteps this entirely: the unpooled
loops already hold the `DiskName` and already call `by_name`, so the compact row
is built where its inputs are in scope -- no reparse, no bridge map.

**Outcome:** one classification point; `braid status`'s JSON, detail, and
compact surfaces cannot contradict on a disk's status by construction; the
bridge map and three duplication-guard tests disappear. Human and JSON output
are byte-identical (`CompactDrive` is render-only, never serialized).

## The change (all in `cli/src/status.rs` unless noted)

### 1. `VerboseContext` -> `DiskViews`, gains a third field

Rename the struct to `DiskViews` and add `compact_drives: Vec<CompactDrive>`
alongside the existing `disks` and `human_details`. The old name was already
inaccurate (it holds the machine-readable `disks` too).

### 2. `build_disk_reports` -> `build_disk_views`

Compute `let alert_devids: HashSet<u64> = pool.alert_missing_devids().into_iter().collect();`
once at the top (the fn already takes `pool`; reuse `PoolState::alert_missing_devids`).

- **Present loop** (the `for (pd, matched_member, disk_name) in present` block,
  already name-sorted): alongside the existing `DiskReport`/`HumanDisk` pushes,
  push `CompactDrive { name: disk_name.clone(), device_short: <pd.underlying
  stripped of "/dev/">, devid: Some(pd.devid), status: DiskStatus::Present }`.
  Copy the `device_short` expression verbatim from today's `build_compact_drives`
  present arm. The push must stay *inside* the sorted loop so present compact
  rows inherit the name order.

- **Unpooled loops** (the `config_disks` arm and the `probe_failures` arm):
  widen the accumulator from `Vec<(DiskReport, HumanDisk)>` to
  `Vec<(DiskReport, HumanDisk, CompactDrive)>`. Each `CompactDrive` is
  `{ name: <cd/failure name>, device_short: "-", devid:
  membership.by_name(&cd.name).and_then(|(_, m)| m.devid).filter(|d|
  alert_devids.contains(d)), status }` where `status` is the row's already-classified
  status (the `status` local in the config arm; `DiskStatus::Unknown` in the
  failure arm). Reuse the `by_name` lookup each arm already performs. Keep the
  single `unpooled.sort_by(|a, _, _| ...)` on `.0.name` (the existing key) and
  drain into all three vecs.

### 3. `build_status`: delete the bridge

Remove the `member_status` HashMap construction and the `build_compact_drives`
call; read `compact_drives` from the `DiskViews` the detail builder now returns.

### 4. Delete `build_compact_drives`

Delete the function and its `member_status` doc block. Keep `present_display_name`
(still shared by the present loop and `build_devid_names`) and `build_devid_names`
(a separate devid->name concern) untouched.

### 5. Pin the set-equality invariant

The folded unpooled set equals "every member that is not a live pool device"
*only because* `build_status` partitions every member into `config_disks` or
`probe_failures` (the `probe_config_disk` loop). Today's compact
`unwrap_or(Missing)` fallback is dead for the same reason and is correctly
dropped. Add a one-line comment at that partition loop noting that
`build_disk_views`' unpooled/compact set-equality depends on every member
becoming a config disk or a probe failure (the loop comment already resists
skipping live members; this names the second consumer).

### 6. Comments & doc

- New `///` on `build_disk_views`: state intent/invariant -- classify each disk
  once and project into `DiskReport` (machine/JSON), `HumanDisk` (verbose),
  `CompactDrive` (always-on summary); single classification point so the three
  `status` surfaces cannot contradict (decision 024); unpooled set = non-pool
  members because `build_status` partitions every member into config-disk or
  probe-failure.
- Add a brief `///` to `CompactDrive`: render-only (not serialized); its status
  is the row's `DiskReport.status` by construction.
- Rewrite the probe-failure loop comment that references
  `build_compact_drives`'s "missing-when-absent fallback": probe failures get a
  cause-neutral `Unknown` row (decision-024 "cannot classify") that projects
  directly into the compact summary; no fallback remains.

### 7. Rename call sites

Update all `build_disk_reports` callers (1 production + ~13 tests) and
`VerboseContext` references to the new names.

## Tests

### Migrate the 7 `build_compact_drives` tests

| Test | Action |
| --- | --- |
| `status_compact_missing_disk` | Rewrite via `build_disk_views`; needs an `Absent` config disk to produce the Missing row. |
| `build_compact_drives_unpooled_member_mirrors_detail_status` | **Delete.** The mirror is now structural (compact status *is* the detail `status` local); a bridge test is meaningless. Replace its intent with the integration invariant below. |
| `status_compact_names_present_disk_from_membership_uuid` | **Delete** -- redundant with present-naming coverage in the detail builder (compact name == detail name via shared `present_display_name`). |
| `status_compact_foreign_mapper_name_does_not_hide_missing_member` | Rewrite to assert on `.compact_drives` (high-value 2-row foreign-mapper case; detail analog already asserts on `.disks`). |
| `build_compact_drives_sorts_present_rows_by_name_not_devid` | Rewrite to assert full `.compact_drives` tuples `(name, device_short, devid, status)` for both present rows and the missing row -- not just `(name, status)` order. Guards the sorted-present-loop ordering *and* the compact-only fields (see note below). |
| `build_compact_drives_missing_member_shows_devid_when_live_confirmed` | **Preserve (rewrite).** Compact-only behavior; not covered by any detail test. |
| `build_compact_drives_missing_member_hides_stale_persisted_devid` | **Preserve (rewrite).** Same, the hide branch. |

The two devid tests are load-bearing: `DiskReport.devid` is `None` for unpooled,
so the alert-filtered devid lives only in the compact projection. Rewrite recipe
(show branch): member `disk_member_with(931, "toshiba3", ".../disk3", Some(3),
None)`; `config_disks = [ConfigDisk { name: "toshiba3", state:
ConfigDiskState::Absent, .. }]`; `pool { missing_devids: vec![3], missing_count:
1, .. }`; empty stats; call `build_disk_views`; assert one compact row with
`devid == Some(3)`, `status == Missing`. Hide branch: `missing_devids: vec![]` ->
`devid == None`.

`device_short` and the alert-filtered unpooled `devid` are the only `CompactDrive`
fields with no counterpart on `DiskReport` (which carries `underlying` and the
btrfs `devid`), so they are absent from the JSON/detail parity tests and from the
bidirectional `(name, status)` `BTreeMap` invariant below. The full-tuple
assertion in the sorts test is their sole guard: it pins present `device_short` =
the `/dev/`-stripped basename (`vda`, `vdb`), unpooled `device_short` = `-`,
present `devid` = `Some(pd.devid)`, and the missing row's alert-filtered `devid`.
Without it, `format_status_human` could regress to `/dev/vda` or drop the `-`
while every other test still passes -- the gap that would otherwise undercut the
"human output byte-identical" claim.

### Fixtures

Add `status_cfg_absent(name, by_id) -> Vec<ConfigDisk>` to
`cli/src/test_fixtures/status.rs`, mirroring the existing
`status_cfg_present_not_luks`, so the two devid tests (and the rewritten
`status_compact_missing_disk`) don't inline the `ConfigDisk { state: Absent }`
literal. Reuse `disk_member_with`, `membership_from`, `test_uuid` from
`test_fixtures/shared.rs`.

### Extend / add

- Extend `status_unpooled_rows_sorted_by_name_across_ok_and_failures` (already
  calls the detail builder) with a `.compact_drives` name-order assertion --
  becomes the canonical "compact unpooled rows interleave Ok/failure by name"
  guard.
- In the existing `build_status` end-to-end test that already inspects
  `extras.compact_drives`, pin the **set-equality invariant bidirectionally**:
  build a `BTreeMap<&str, DiskStatus>` of the non-present (`status != Present`)
  rows of `report.disks` and another of the non-present rows of
  `extras.compact_drives`, then `assert_eq!` the two maps. Map equality is
  inherently two-way and asserts equal status per name -- it catches a *dropped*
  compact row whose detail row survives (the silent-drop regression a one-way
  "every compact row has a detail row" check would miss) *and* an extra or
  misclassified compact row. This is the main fold invariant (section 5): the
  non-present compact set must equal the non-present detail set, guarding e.g. a
  future "skip live-member probe" optimization. (`DiskStatus` is `Copy +
  PartialEq`, so both maps and the `assert_eq!` are trivial to build.)

## Out of scope / unaffected

- `build_devid_names` and `present_display_name`: unchanged.
- `--json` output: `CompactDrive` is not serialized (`StatusReport.disks` carries
  the detail rows); JSON bytes cannot change. Existing JSON tests still hold.
- Decision 024: already documents this invariant ("Display code has an explicit
  join rule"); no doc edit needed.

## Verification

1. `just test-rust` -- compiles the rename across all call sites and runs the
   migrated/added unit tests. The human "Drives:" output is byte-identical along
   all three axes: the present/unpooled set-and-order equality (sort tests + the
   bidirectional `BTreeMap` invariant) covers which rows render, in what order,
   with what status; the full-tuple sorts test additionally pins the compact-only
   rendered field values (`device_short`, alert-filtered `devid`) that have no
   `DiskReport`/JSON counterpart.
2. `rg CompactDrive cli/src` -- confirm `status.rs` is still the only consumer
   (sole renderer is `format_status_human`).
3. No fixture/parser impact (no `nixpkgs`/tool change), so
   `just capture-all-fixtures` / `just test-parsers` are not required.
4. Optional end-to-end: `tests/cli/braid-status-rust.py` already pins real
   `status` human + JSON output; no new VM test is needed since behavior is
   unchanged, but it is the backstop if run.

## Implementation notes

- Section 7 said "update all `build_disk_reports` callers"; I extended the
  rename to the `build_disk_reports_*` test-function *names* as well, since
  leaving them would dangle a deleted symbol in test identities. The migrated
  compact sort test is named `build_disk_views_sorts_present_compact_rows_by_name_not_devid`
  (the `_compact_` infix) to avoid colliding with the existing detail
  `build_disk_views_sorts_present_rows_by_name_not_devid`.
- Deleting `build_compact_drives` left a stale reference in the
  `status_unpooled_probe_failure_renders_unknown` test preamble (its "Why it
  exists" cited the missing-when-absent fallback). The plan only named the
  builder's probe-failure loop comment (section 6), so I also rewrote that
  preamble to describe the fold (compact verdict == detail verdict by
  construction) rather than the removed fallback.
