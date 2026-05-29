# Plan: pin the present-disk membership-joined `by_id` rendering

## Context

A code-review finding flagged that the canonical status integration fixture
`build_healthy_status` saves no `pool.json`, so the dominant integration suite renders
present disks via the empty-membership fallback (mapper basename + `/dev/mapper/...`) rather
than the production membership join. Investigation confirmed the fixture is empty-membership,
but the proposed fix (convert the canonical fixture, flip 8 tolerance assertions, revive
"dead" lsblk mocks) was over-scoped and rested on a false premise. Ground-truth re-reads:

- **lsblk Model/Serial is membership-independent.** `build_disk_reports` reads them from
  `pd.underlying` (the live backing path), not the joined `by_id` (status.rs:984-987). The
  verbose runner's lsblk mocks are keyed `/dev/vda|vdb|vdc` (test_fixtures/status.rs:540-578) =
  `pd.underlying`, so Model/Serial already render today. There are no "dead" mocks to revive,
  and asserting Model/Serial proves nothing about the join.
- **The name join is already covered.** `present_display_name` is shared across compact/verbose/
  devid rendering (status.rs:241-249) and pinned by the compact unit test
  (`status_compact_names_present_disk_from_membership_uuid`, status.rs ~5540) plus VM tests:
  `braid-status-rust.py:135` (JSON, intact), `:186` (degraded), and human compact output in
  `status-mapper-drift.py:73-94`.
- **The empty-membership fixture is real coverage, not a defect.** A mounted pool with a
  missing/corrupt `pool.json` is a genuine recovery state, and the fallback rendering is what
  the operator sees then. The 8 `build_status` tolerance tests incidentally pin it via
  `assert_common_human_sections_survived` (status.rs:3602-3618). Converting the fixture would
  delete that coverage and -- per the review -- still not be production-faithful (pool mappers
  stay `disk1`, not `braid-toshiba1`, and are probed closed on a mounted pool).

**The one genuinely unguarded behavior** is the present-disk `by_id` join arm
(status.rs:978-980): for a present member, `by_id = member.by_id`; otherwise the
`/dev/mapper/{mapper}` fallback. No test asserts the joined value -- the VM test only checks
`"by_id" in d` (presence, braid-status-rust.py:118), and the compact path doesn't render
`by_id` at all. A regression collapsing present `by_id` to the fallback would slip through.

**Outcome:** add one focused fast-suite test (plus a one-line VM assertion) that pins the
present-disk membership-joined `by_id` value and its verbose-human `Device:` line, while
leaving the tolerance suite and its missing-`pool.json` fallback coverage untouched.

## Approach

Add a single membership-populated `build_status` integration test in the fast Rust suite that
asserts the production join, and tighten one existing VM assertion. Operator names
(`toshiba1/2/3`) are chosen distinct from the live mapper basenames (`disk1/2/3`) so the
rendered name/by-id can only appear via the UUID join, never the fallback -- the test is a true
discriminator.

Because saving membership makes `build_status` run `probe_config_disk` per member
(status.rs:524-549, errors via `?`), the runner needs per-member probe mocks. For an inactive
mapper, `classify_mapper_ownership` short-circuits after the single `CryptsetupStatus` call
(luks.rs:847-851) -- no backing-path or lsblk probes -- so the only additions are LUKS2
`luksDump` + closed-mapper `status`, for all three present members.

### 1. Fixture helper -- `cli/src/test_fixtures/status.rs`

Add `status_membership_3disk()` mirroring `status_membership_1disk()` (line 117), keyed by the
exact pool UUIDs the healthy runner emits, with devids set (faithful, and lets the
missing-device-banner test dedup its inline construction at status.rs:4538-4554 later):

```rust
/// Three-disk membership for present-disk identity-rendering tests. Operator
/// names toshiba1/2/3 are deliberately distinct from the disk1/2/3 mapper
/// basenames so a rendered name/by-id can only come from the UUID join, not the
/// fallback. Keyed by the pool UUIDs status_runner_healthy_3disk_base emits.
pub(crate) fn status_membership_3disk() -> PoolMembership {
    let mut m = PoolMembership::empty();
    for (seed, name, by_id, devid, uuid) in [
        (1, "toshiba1", "/dev/disk/by-id/disk1", 1u64, "11111111-1111-1111-1111-111111111111"),
        (2, "toshiba2", "/dev/disk/by-id/disk2", 2,    "22222222-2222-2222-2222-222222222222"),
        (3, "toshiba3", "/dev/disk/by-id/disk3", 3,    "33333333-3333-3333-3333-333333333333"),
    ] {
        let (_, member) = disk_member_with(seed, name, by_id, Some(devid), None);
        m.insert(LuksUuid::parse(uuid).unwrap(), member).expect("insert member");
    }
    m
}
```

Wiring: add `disk_member_with` to the `use super::shared::{...}` import (test_fixtures/status.rs:14);
re-export `status_membership_3disk` from `test_fixtures.rs` (next to `status_membership_1disk`,
~line 219); import it in the status.rs test module (~line 1461). `PoolMembership`/`LuksUuid` are
already in scope in the fixture module.

### 2. The focused test -- `cli/src/status.rs` (beside the missing-device banner test, ~4529)

```rust
// Intent: a present pool member renders its persisted by-id path (and operator
//   name) via the LUKS-UUID membership join, not the mapper-basename fallback.
// Why it exists: the by_id arm of build_disk_reports (status.rs:978-980) is the
//   only present-disk join behavior nothing pins by value -- the VM suite only
//   checks by_id is present, and the name join is covered elsewhere. A
//   regression collapsing present by_id to the /dev/mapper fallback would slip.
// Scenario: a healthy 3-disk mounted pool whose pool.json operator names
//   (toshiba*) differ from the live mapper basenames (disk*).
#[test]
fn build_status_present_member_renders_by_id_and_operator_name() {
    let runner = status_runner_healthy_3disk_verbose(status_runner_healthy_3disk_base())
        .with_luks_dump_text_luks2_for(&[
            "/dev/disk/by-id/disk1",
            "/dev/disk/by-id/disk2",
            "/dev/disk/by-id/disk3",
        ])
        .with_mappers_closed(&["braid-toshiba1", "braid-toshiba2", "braid-toshiba3"]);
    let fs = status_fs_three_disk();
    let config = status_config();
    let (_tmp, paths) = isolated_paths();
    membership::save_membership(&status_membership_3disk(), &paths).unwrap();

    let built = build_status(
        &runner, &fs, &config, &paths,
        crate::test_fixtures::mock_virtio_backing_path_resolver(),
    )
    .expect("membership-populated healthy status should build");
    let human = render_built_status(&built);

    // Verbose "Disks:" section: by-id Device line + operator-name header.
    assert!(human.contains("    Device:  /dev/disk/by-id/disk1"), "got:\n{human}");
    assert!(human.contains("  toshiba1"), "got:\n{human}");

    // Structured: present by_id is the member by-id (not the /dev/mapper
    // fallback); mapper stays the live btrfs mapper basename.
    let d1 = built.report.disks.iter().find(|d| d.name == "toshiba1")
        .expect("toshiba1 present row");
    assert_eq!(d1.by_id, "/dev/disk/by-id/disk1");
    assert_eq!(d1.mapper, "disk1");
}
```

Notes:
- The **verbose runner is required** (not just base): `probe_config_disk` calls
  `CryptsetupLuksUuid{/dev/disk/by-id/diskN}` per member, which the verbose runner mocks
  (test_fixtures/status.rs:505-531); the base runner lacks them.
- **All three** members need `luksDump` + closed-mapper mocks because `status_fs_three_disk`
  includes all three by-id paths, so none short-circuit as `Absent` (probe.rs:164-169).
- **No `Model:`/`Serial:` assertions** -- they read `pd.underlying` and are membership-independent.
- `render_built_status` (status.rs:3584) and `membership_from`/`disk_member_with` are already in
  the test module.

### 3. VM complement -- `tests/cli/braid-status-rust.py` (Healthy JSON loop, ~130-141)

Tighten the `by_id` check from presence to value at the highest-fidelity layer (one line). Disks
are added as `<name>=/dev/disk/by-id/virtio-<name>` (add_disk, line 29), so:

```python
assert by_uuid[uuid]["by_id"] == f"/dev/disk/by-id/virtio-{name}", (
    f"{name} by_id must render the membership by-id path, got {by_uuid[uuid]['by_id']!r}"
)
```

(The blanket `"by_id" in d` presence check at line 118 becomes subsumed; leave it or drop it.)

## What stays unchanged (and why)

- `build_healthy_status`, `build_healthy_status_with_output`, `assert_common_human_sections_survived`,
  and the 8 tolerance tests: **untouched**. Their intent is probe-failure tolerance (unrelated to
  membership), and they correctly exercise the missing/empty-`pool.json` fallback rendering, which
  is a real recovery scenario worth keeping covered. Not converting them avoids coupling 8 unrelated
  tests to the per-member probe-mock surface.

## Why this shape (not the conversion)

Once the lsblk premise is corrected and the existing name-join coverage is credited, the conversion's
only remaining argument is decision-024 faithfulness -- which Finding C shows the converted fixture
wouldn't actually achieve, and which the missing-`pool.json` scenario makes a false dichotomy
(braid wants both states covered). The focused test closes the single real gap (present `by_id`
value + its verbose `Device:` line) as a sibling to the existing fallback coverage, in the fast suite
where pure rendering logic belongs, with no blast radius on the tolerance suite.

## Critical files

- `cli/src/status.rs` -- new `build_status_present_member_renders_by_id_and_operator_name` test; test-module import of `status_membership_3disk`.
- `cli/src/test_fixtures/status.rs` -- new `status_membership_3disk()`; `disk_member_with` import.
- `cli/src/test_fixtures.rs` -- facade re-export of `status_membership_3disk`.
- `tests/cli/braid-status-rust.py` -- tighten the Healthy-JSON `by_id` assertion to a value check.
- Reference only (no edits): `cli/src/probe.rs:157-223` (probe_config_disk mock contract),
  `cli/src/luks.rs:835-915` (classify_mapper_ownership inactive short-circuit),
  `cli/src/cmd.rs:1504,1529` (with_luks_dump_text_luks2_for / with_mappers_closed),
  `cli/src/status.rs:978-987` (the by_id join arm under test; lsblk reads pd.underlying).

## Verification

1. `just test-rust` -- the new test passes; all existing status tests stay green (no shared fixture
   or assertion changed).
2. Discriminator proof (mutation check, revert after): temporarily drop the `.map` arm at
   status.rs:978-980 so present `by_id` always falls back -> the new test's
   `Device:  /dev/disk/by-id/disk1` and `d1.by_id` assertions must FAIL. Temporarily make
   `present_display_name` ignore the member -> the `  toshiba1` assertion must FAIL.
3. `just test-vm braid-status-rust` -- the tightened `by_id`-value assertion passes against the
   live virtio by-id paths. (Run this single check; the full VM suite is 20-30 min and unaffected.)
4. No fixture-refresh / parser-canary obligation: no parser-critical tool versions change.

## Optional follow-up (not required)

Collapse the missing-device-banner test's inline membership (status.rs:4538-4554) to reuse
`status_membership_3disk()` -- identical UUIDs/names/devids. Defer unless touching that test anyway.
