# Plan: Stop suggesting `braid replace` for foreign live mappers

## Context

The per-disk `Disks:` section of `braid status` human output (always
emitted when the pool is mounted -- there is no `-v` flag; `StatusArgs`
only has `--json`, see `cli/src/main.rs:218-221`) emits a destructive
Action hint that violates the LUKS-UUID identity boundary set by
[decision 024](../../docs/decisions/024-luks-uuid-identity.md) and
[principle 5](../../docs/principles.md#5-stable-identifiers).

When `build_disk_reports` cannot join a live btrfs pool device back to
membership by LUKS UUID, the row falls back to the raw mapper basename
for `disk_name`
(`cli/src/status.rs:743-751`). That fallback is intentionally
display-only -- recent commit `b026516` ("fix(identity): keep status
joins uuid-keyed") added it specifically so foreign live mappers stay
visible without being silently treated as members.

The per-disk Action hint never caught up. The branch at
`cli/src/status.rs:1114-1118`:

```rust
if has_errors || d.status == DiskStatus::Missing {
    out.push_str(&format!(
        "    Action:  add replacement disk to config, then: braid replace --old {} --new <new-name>\n",
        d.name
    ));
}
```

unconditionally interpolates `d.name`. For a foreign live mapper with
non-zero btrfs error counters (read/write/flush/corruption/generation),
this prints, e.g., `braid replace --old braid-disk1 --new <new-name>`
-- nudging the operator to target a runtime handle as if it were a
member's identity. That is the exact failure mode decision 024
forbids.

The fix: type-encode "this row was joined to membership" so the Action
emit site cannot reuse the display string by accident. Foreign rows
get a non-destructive `braid doctor` redirect instead.

## Design

Add a typed `member_name: Option<DiskName>` field on `HumanDisk` (the
internal verbose-row carrier defined at `cli/src/status.rs:293-302`).

- `Some(DiskName)` whenever the row is membership-joined. The two
  non-test construction sites both have a member in hand:
    - `cli/src/status.rs:790-799` (present-pool-devices loop):
      `member_name: matched_member.map(|m| m.name.clone())` -- this
      is the only call site where the value can ever be `None`,
      and only on the foreign-mapper fallback path.
    - `cli/src/status.rs:851-860` (unpooled-config-disks loop):
      `member_name: Some(cd.name.clone())` -- these rows are always
      members.
- Branch on it at the Action-hint emit site:

```rust
if has_errors || d.status == DiskStatus::Missing {
    match &d.member_name {
        Some(name) => out.push_str(&format!(
            "    Action:  add replacement disk to config, then: braid replace --old {name} --new <new-name>\n",
        )),
        None => out.push_str(
            "    Action:  foreign mapper detected -- run 'braid doctor' to investigate\n",
        ),
    }
}
```

`DiskName`'s `Display` impl (`cli/src/types.rs:152-156`) renders the
raw inner string, so the typed interpolation produces identical text
to today on the `Some` arm.

### Why `Option<DiskName>` rather than `is_member: bool`

A bool would force the Action callsite to re-trust `d.name` (a
display-only `String`) for the argv. The whole point of decision 024
is to make member identity unreconstructable from runtime handles;
encoding the constraint in the type system is the smallest change that
matches the codebase's existing typed-identifier discipline
(`DiskName`, `LuksUuid`, `ByIdPath`, `MapperName` are all newtypes).
`DiskName` is already in scope via `use crate::types::*;` at
`cli/src/status.rs:21`.

### Why this fix stays local to the human path

- `DiskReport.name: String` is the JSON contract. Foreign-mapper
  rows in JSON are arguably correct surface (it's what `lsblk` shows
  for that mapper). Re-shaping `DiskReport` is a larger decision that
  doesn't belong in this fix.
- The compact `Drives:` listing
  (`cli/src/status.rs:980-991`, fed by `build_compact_drives` at
  `219-258`) and `devid_to_name` (`873-881`) interpolate the same
  string but only for informational display -- no destructive
  command is suggested. They stay as-is.
- The other in-crate "braid replace --old" emitters
  (`doctor.rs:620`, `remove_missing.rs:446`) use literal `<disk>` /
  `<missing-name>` placeholders, never an interpolated runtime name.
  No sibling fix needed.

### Action text

`foreign mapper detected -- run 'braid doctor' to investigate` uses
`--` (double hyphen) per the CLI output style guideline in
`AGENTS.md`, matching the existing precedent at
`cli/src/status.rs:1120` (`run 'braid doctor' for recovery guidance`).

## Touch points

Files to modify:

- `cli/src/status.rs`
    - `HumanDisk` struct (line ~293): add `member_name: Option<DiskName>` field.
    - Present-pool-devices loop (line ~790): set
      `member_name: matched_member.map(|m| m.name.clone())`.
    - Unpooled-config-disks loop (line ~851): set
      `member_name: Some(cd.name.clone())`.
    - Verbose Action-hint emit (lines 1114-1118): branch on
      `member_name` as shown above.
    - Eight existing in-file test sites that build `HumanDisk { ... }`
      inline must populate the new field. None of them currently
      assert on `Action:` text, so the change is mechanical:
        - `cli/src/status.rs:1813` (`status_verbose_present_disks`)
        - `cli/src/status.rs:1864` (`status_verbose_missing_disk`)
        - `cli/src/status.rs:1917`, `1979`, `2027`, `3406` (other verbose tests)
        - any remaining inline constructions in `#[cfg(test)]`

No external fixture exposes `HumanDisk`, so no test-helper module
needs to change.

## Verification

### New unit test (drives both the join and the format path)

A formatter-only test that hand-builds `HumanDisk { member_name, .. }`
rows would pass even if `build_disk_reports` accidentally set
`member_name: Some(...)` for a foreign mapper -- the actual failing
join path would never be exercised. The regression test must drive
`build_disk_reports` end-to-end.

In the existing `#[cfg(test)] mod tests` of `cli/src/status.rs`, add a
test (preamble in the project's three-line `Intent / Why it exists /
Scenario` form per `AGENTS.md`). Model it on the existing
`disk_report_pairs_stats_by_devid_when_path_differs` test at
`cli/src/status.rs:3777-3832`, which already shows how to drive
`build_disk_reports` with a real `BtrfsDeviceStatsOutput` carrying
non-zero `read_io_errs`. Reuse the `status_membership_1disk` fixture
for the member side.

Scenario:

- Membership snapshot has exactly one member (`disk1` at UUID `U1`).
- `PoolState` has one `PoolDevice` with mapper `braid-disk1`, devid 1,
  and a **foreign** `luks_uuid` (e.g. `99999999-...`) -- so the
  UUID join in `build_disk_reports` fails and the row falls back to
  `member_name: None`.
- `BtrfsDeviceStatsOutput` carries one stats row for devid 1 with
  `read_io_errs: 5` (non-zero), driving `has_errors == true`.
- Call `build_disk_reports(&runner, &membership, &config_disks, &pool,
  &stats)` to obtain `ctx`.
- Build a minimal `StatusReport` (mounted, degraded; see the patterns
  at `cli/src/status.rs:1830-1851` for shape) and call
  `format_status_human(&report, None, Some(&ctx.human_details))`.

Assertions:

- The foreign-mapper row exists with `name == "braid-disk1"` and
  `member_name.is_none()` (pin the construction-side invariant
  directly on `ctx.human_details`, in addition to the format-side
  assertions below).
- The formatted output does NOT contain the substring `braid replace
  --old braid-` anywhere -- the foreign-mapper arm cannot leak a
  destructive command.
- The formatted output contains both `foreign mapper detected` and
  `run 'braid doctor'` -- the redirect renders.
- The same test (or a sibling test using the same fixture style)
  also exercises the present-member arm: a second `PoolDevice` whose
  `luks_uuid` matches the membership UUID, with non-zero
  `read_io_errs` for its devid, and asserts the formatted output
  contains `braid replace --old disk1 --new <new-name>`. This pins
  the `Some(DiskName)` arm for live-present rows.

### Sibling unit test: pin Missing-member construction

The Action branch in `format_status_human` fires on both
`has_errors` AND `d.status == DiskStatus::Missing`. Missing rows
come from a different `build_disk_reports` arm -- the unpooled
config-disks loop at `cli/src/status.rs:851-860` -- and the fix sets
`member_name: Some(cd.name.clone())` there. If a future change
accidentally set that to `None`, true missing disks would be routed
to the foreign-mapper doctor redirect instead of the recovery hint.
The foreign-mapper and present-member tests above would still pass.
Existing `status_verbose_missing_disk` (`cli/src/status.rs:1862-1900`
range) does not assert the `Action:` text, and the
`braid-status-rust` VM test does not exercise this hint either.

Add a sibling `build_disk_reports` unit test:

- `status_membership_1disk()` provides one member `disk1`.
- `status_pool_empty()` (`cli/src/test_fixtures/status.rs:573`) for
  the live pool -- no `PoolDevice` exists.
- One `ConfigDisk { name: DiskName::parse("disk1").unwrap(),
  by_id_path: ByIdPath::parse("/dev/disk/by-id/disk1").unwrap(),
  state: ConfigDiskState::Absent }`. The `Absent` arm at
  `cli/src/status.rs:811-812` maps to `DiskStatus::Missing`.
- Call `build_disk_reports`; assert `ctx.human_details[0].member_name
  == Some(DiskName::parse("disk1").unwrap())` and
  `ctx.human_details[0].status == DiskStatus::Missing`.
- Build a minimal `StatusReport` (degraded; `missing_count: Some(1)`),
  call `format_status_human(&report, None,
  Some(&ctx.human_details))`, and assert the formatted output
  contains `braid replace --old disk1 --new <new-name>` -- the
  recovery hint must render via the `Some(DiskName)` arm even when
  `has_errors` is false and only `Missing` triggers the branch.

### Regression sweep

- `just test-rust` -- all existing status unit tests still pass
  after populating the new field.
- `just test-vm braid-status-rust` -- end-to-end Rust status VM test
  (see `flake.nix:221-225`, `tests/cli/braid-status-rust.nix`)
  exercises real `braid status` output against real LUKS + btrfs
  member rows. Confirm the human output for healthy/missing pools
  is unchanged.

### No changes required to:

- `docs/decisions/024-luks-uuid-identity.md` (no invariant change;
  this fix brings code into alignment with the existing decision).
- `docs/principles.md`.
- `README.md` (user-facing behavior change is restricted to a
  diagnostic on an unusual configuration; not worth a cookbook
  entry).
