# Fix missing-device alert banner drops disk name

## Context

`braid status` renders an alert banner like `missing device: devid 3` for
btrfs-MISSING and null-underlying devids -- exactly the case the alert is
designed to surface. The operator-facing disk name is dropped because
`devid_to_name` (`cli/src/status.rs:873-881`) only joins through
`report.disks`, and the only matching row for an unpooled member is the
unpooled-member `DiskReport` produced at `cli/src/status.rs:840-849`,
which hard-codes `devid: None`. Principle 2 (`docs/principles.md:17`)
and decision 024 (`docs/decisions/024-luks-uuid-identity.md:21-24`)
explicitly authorize using persisted `DiskMember.devid` as the
fallback join key for these two cases. The TUI already does this
correctly at `cli/src/tui/probe.rs:55-81`; the CLI status banner
regressed when `devid_to_name` was introduced in commit `d841151`.

The same root cause also strips the devid column from the compact
`Drives:` listing for missing rows (`build_compact_drives` at
`cli/src/status.rs:243-255` writes `devid: None`; the renderer at
`cli/src/status.rs:983-986` shows `-`). Both losses are fixed by one
join.

Existing tests (`alert_missing_device_shows_name` at
`cli/src/status.rs:3462-3474`, `cmd_status_degraded_ok` at
`cli/src/status.rs:2737-2827`) miss the bug because the first only
exercises `Present`/`Some(devid)` rows via `status_disk_report_named`
(`cli/src/test_fixtures/status.rs:640-651`) and the second only asserts
`result.is_ok()`.

Intended outcome: the banner reads `missing device: toshiba3 (devid 3)`
and the compact listing shows `devid=3` for a missing member whose
persisted devid lines up with live btrfs's MISSING/null-underlying set.

## Design

### 1. Build a unified `devid -> display name` map in `build_status`

Replace the implicit join through `report.disks` with an explicit
`HashMap<u64, String>` covering all three live-btrfs sources, mirroring
the `disk_name` resolution that `build_disk_reports` already does at
`cli/src/status.rs:743-751`:

- Live `pool.devices`:
  `(pd.devid, membership.by_uuid(&pd.luks_uuid).map(|m| m.name.as_str().to_owned()).unwrap_or_else(|| pd.mapper.0.clone()))`.
  Foreign live devices with no membership match keep the
  raw-mapper-basename display fallback, so a `BtrfsDeviceErrors` alert
  on an unmatched live mapper still renders
  `btrfs device errors on <mapper> (devid N)` -- never bare `devid N`.
- `pool.null_underlying`: `(nu.devid, ...name)` from
  `membership.by_devid(nu.devid)`. If the persisted-devid lookup
  returns `None`, omit the entry (banner falls back to `devid N`).
  No raw-mapper fallback here -- the principle-2 fallback is the
  persisted devid binding, and `null_underlying` rows don't have an
  observed-name path through any membership we trust.
- `pool.missing_devids`: same as null-underlying.

The map value is a plain `String`, not `DiskName`, because the
foreign-live-mapper fallback above produces a mapper basename that
is not a valid `DiskName`. The map represents "the string the user
should see next to `(devid N)` in display contexts," not membership
identity.

`PoolMembership::by_devid` already exists at `cli/src/membership.rs:284-302`
and returns `Result<Option<(&LuksUuid, &DiskMember)>, MembershipError>`.
Propagate `MembershipError::DuplicateDevid` via the existing
`StatusError::Membership(#[from] membership::MembershipError)` arm at
`cli/src/status.rs:277` -- matches the fail-closed stance at
`cli/src/status.rs:362-370`.

New helper, sibling to `build_compact_drives` and `build_disk_reports`:

```rust
/// Resolve every devid live btrfs surfaces (present, null-underlying,
/// btrfs-MISSING) back to its operator-facing display name. Mirrors
/// `build_disk_reports`'s `disk_name` rule for present devices
/// (UUID-matched member name, else observed mapper basename) and uses
/// persisted `DiskMember.devid` only as the principle-2 fallback for
/// null-underlying and btrfs-MISSING entries, where no live LUKS UUID
/// is observable.
fn build_devid_names(
    pool: &PoolState,
    membership: &PoolMembership,
) -> Result<HashMap<u64, String>, MembershipError> { ... }
```

### 2. Plumb the map through `MountedExtras`

Extend `MountedExtras` (`cli/src/status.rs:313-316`):

```rust
struct MountedExtras {
    compact_drives: Vec<CompactDrive>,
    human_details: Vec<HumanDisk>,
    devid_names: HashMap<u64, String>,
}
```

`format_status_human` gains a fourth parameter
`devid_names: Option<&HashMap<u64, String>>`. `cmd_status`
(`cli/src/status.rs:444-452`) passes
`extras.map(|e| &e.devid_names)`.

### 3. Rewrite `devid_to_name`

```rust
fn devid_to_name(
    devid_names: Option<&HashMap<u64, String>>,
    devid: u64,
) -> String {
    devid_names
        .and_then(|m| m.get(&devid))
        .map(|name| format!("{name} (devid {devid})"))
        .unwrap_or_else(|| format!("devid {devid}"))
}
```

The old `report.disks` lookup at `cli/src/status.rs:875-880` (and its
awkward `d.devid.as_deref() == Some(&key)` string-parse) is removed --
the map is now the single source of truth for the join. Call sites at
`cli/src/status.rs:897-902` pass the new parameter instead of `report`.

### 4. Fix `build_compact_drives` for missing members

In the unpooled-membership branch at `cli/src/status.rs:244-255`,
surface the member's persisted devid only when live btrfs confirms it
(principle 2: displayed devids must be live-btrfs-authoritative):

```rust
let alert_devids: HashSet<u64> =
    pool.alert_missing_devids().into_iter().collect();
for (uuid, member) in membership.iter_by_name() {
    if pool_luks_uuids.contains(uuid) {
        continue;
    }
    let devid = member.devid.filter(|d| alert_devids.contains(d));
    drives.push(CompactDrive {
        name: member.name.as_str().to_owned(),
        device_short: "-".to_owned(),
        devid,
        status: DiskStatus::Missing,
    });
}
```

A persisted devid that is *not* in `alert_missing_devids()` (i.e. the
member is unpooled but live btrfs has no MISSING/null-underlying record
of it -- e.g. fully detached, never assembled) stays `None`. This keeps
display authority on live btrfs.

## Critical files

- `cli/src/status.rs` -- add `build_devid_names`; extend `MountedExtras`;
  rewrite `devid_to_name`; thread the map through `format_status_human`
  and `cmd_status`; update `build_compact_drives` unpooled branch;
  populate `devid_names` in `build_status` (`cli/src/status.rs:342-426`).
- `cli/src/test_fixtures/status.rs` -- add a `status_disk_report_missing(name)`
  helper next to `status_disk_report_named` (so tests can build the
  unpooled-row shape with `devid: None`); extend
  `status_report_with_alerts` or add a sibling that also lets tests pass
  a `devid_names` map through.

## Reused code, no new abstractions

- `PoolMembership::by_devid` (`cli/src/membership.rs:284-302`).
- `PoolMembership::by_uuid` (`cli/src/membership.rs:259-261`).
- `PoolState::alert_missing_devids` (`cli/src/types.rs:384-392`).
- `StatusError::Membership` From impl (`cli/src/status.rs:277`).
- The TUI helper at `cli/src/tui/probe.rs:55-81` is the architectural
  precedent. Not unified in this fix: its upstream inputs are different
  (`HashMap<String, LuksUuid>` + `HashMap<String, u64>` vs.
  `&PoolMembership`). Note in the doc comment that a future refactor
  could collapse both into one helper.

## Tests

Rust unit tests in `cli/src/status.rs` (mod tests). Each test starts
with the three-section preamble per `docs/testing.md`.

1. `alert_missing_device_uses_devid_names_map` -- banner renders the
   member name for `MissingDevice { devid: 3 }` when `report.disks` has
   no matching row but the passed `devid_names` map carries
   `{3: "toshiba3"}`. This is the regression test for the bug.
2. `alert_missing_device_falls_back_when_map_missing_entry` -- banner
   falls back to `"missing device: devid 99"` when the devid is absent
   from `devid_names`. (Strengthens the existing
   `alert_unknown_devid_falls_back` to assert against the new code
   path.)
3. `alert_btrfs_errors_foreign_live_mapper_keeps_basename` --
   `BtrfsDeviceErrors { devid: 1 }` on a present live device whose
   LUKS UUID does NOT match any membership entry. `devid_names` map
   carries `{1: "<mapper-basename>"}` (built via the
   `build_disk_reports`-style fallback for foreign live devices).
   Assert the banner contains
   `btrfs device errors on <mapper-basename> (devid 1)`, not bare
   `devid 1`. Regression guard for the F1 finding.
4. `build_devid_names_covers_present_null_underlying_and_missing` --
   given a `PoolState` with one live device, one null-underlying devid,
   and one btrfs-MISSING devid, plus a matching membership, returns a
   3-entry map with the correct devid -> name binding for each row.
5. `build_devid_names_present_foreign_live_uses_mapper_basename` --
   live `pool.devices` entry whose LUKS UUID is not in membership
   produces a map entry `(devid, pd.mapper.0)` (matches the foreign
   live device fallback path in `build_disk_reports` at
   `cli/src/status.rs:746-751`).
6. `build_devid_names_propagates_duplicate_devid` -- corrupt membership
   where two members share the same persisted devid (in the
   missing/null-underlying lookup path) returns
   `MembershipError::DuplicateDevid` rather than silently picking one.
7. `build_compact_drives_missing_member_shows_devid_when_live_confirmed`
   -- unpooled member whose persisted `devid` is in
   `pool.alert_missing_devids()` -> `CompactDrive.devid = Some(N)`.
8. `build_compact_drives_missing_member_hides_stale_persisted_devid` --
   unpooled member whose persisted `devid` is NOT in
   `pool.alert_missing_devids()` -> `CompactDrive.devid = None`.
   (Principle 2: no stale devids in display.)
9. `build_status_missing_device_banner_and_compact_row_name_member_end_to_end`
   -- full `build_status` + `format_status_human` path with a probe
   that produces 1 btrfs-MISSING devid (devid 3), saved membership
   carrying the persisted devid binding (`toshiba3` -> devid 3),
   latched `MissingDevice` cause. Assert the rendered human output
   contains BOTH:
   - the banner line `missing device: toshiba3 (devid 3)`, and
   - a `Drives:` row for `toshiba3` whose devid column reads `devid=3`
     (not `-`).
   This locks the second user-facing outcome (compact-listing devid
   surfaced for missing rows) end-to-end -- without it, a
   formatter/plumbing regression could leave compact-listing rendering
   `-` while the builder unit tests #7-#8 still pass. Mirrors the
   assembly used by
   `build_status_missing_devids_unions_btrfs_missing_and_null_underlying`
   at `cli/src/status.rs:2860+`.

Tests use the existing fixtures (`status_btrfs_show_*`,
`status_membership_*`, `status_report_with_alerts`,
`status_disk_report_named`) plus the small new helpers noted in
"Critical files".

## Out of scope

- Unifying the new helper with `cli/src/tui/probe.rs:55-81`. Worth
  doing as a follow-up; not required to fix the bug.
- Changing the JSON shape of `DiskReport` or `CompactDrive`. The fix
  does not pre-populate `DiskReport.devid` from membership; principle 2
  reserves the displayed devid field for live-btrfs authority. The
  human-side `devid_names` map is the join; JSON consumers see only
  what they see today (and gain `pool.missing_devids` already, which
  carries the devid list for missing rows).
- Removing the verbose-listing "Missing -> no devid shown" behavior at
  `cli/src/status.rs:1040`. That intentionally collapses missing rows to
  a single `MISSING` line; not the same UX target as the banner.

## Verification

End-to-end checks before commit:

1. `just test-rust` -- new unit tests pass; existing
   `alert_missing_device_shows_name`, `alert_btrfs_errors_shows_name`,
   `alert_unknown_devid_falls_back`, and
   `build_status_missing_devids_unions_*` still pass.
2. `just test-vm cmd-status` (or whichever VM check exercises
   `braid status` against a degraded pool -- pick the closest one to
   the missing-device path) confirms the banner and compact listing
   render correctly against real btrfs/cryptsetup output. If no such
   VM test exists today, the integration unit test (#7 above) covers
   the assembly path and we file a follow-up to add a VM test.
3. Manual diff review against principle 2: confirm `DiskReport.devid`
   in JSON output is still only populated from live `pool.devices`,
   not from persisted membership.
4. `cargo clippy` clean.
