# Pivot `cmd_status_degraded_ok` into a faithful degraded `build_status` content test

## Context

A review flagged that `cmd_status_degraded_ok` (`cli/src/status.rs`) wires a full
degraded mock (3-device pool, 1 missing) but asserts only `result.is_ok()` -- it
proves the pipeline doesn't panic, nothing about the degraded report. The finding
claimed this was a "misleading-claim smell, not a true coverage hole" because the
degraded contract is held by `status_json_degraded`, `status_human_degraded`, and
the VM test's Phase 3.

Investigation refined that: the gap is **real, just small**.

- `status_json_degraded` / `status_human_degraded` hand-construct a `StatusReport`
  literal and test only serialization / `format_status_human`. They set
  `status`/`present_count`/`missing_count` directly, so they **cannot** catch a
  `build_status` misclassification.
- Grep of `built.report.status` assertions: only `Intact` (4 sites) and
  `NotMounted` (3 sites) -- **zero** `Degraded`. `present_count` is asserted as
  `Some(3)` (healthy) and `Some(1)` (single-disk), **never** `Some(2)` (degraded).
- `build_status_missing_devids_unions_btrfs_missing_and_null_underlying`
  (`cli/src/status.rs`) asserts `missing_count` for a *different* fixture (2-missing,
  mixed null-underlying), not `status` or `present_count`.
  `build_status_missing_device_banner_and_compact_row_name_member_end_to_end` asserts
  the alert banner + a compact/detail fold invariant, and uses the
  *explicit-MISSING-placeholder* fixture -- a different probe branch.
- The `status_btrfs_show_3disk_1missing()` fixture (the `*** Some devices missing`,
  total-minus-listed branch) is exercised **only** by this vacuous smoke test.

So `StatusCode::Degraded` + degraded counts are never pinned by a fast test -- only
by the slow VM lane. Separately, because the smoke test saves **no** membership,
`config_disks` is empty, so its three `by-id` `CryptsetupLuksUuid` mocks are dead
wiring and the missing member produces no disk row at all.

**Outcome:** replace the smoke test with a `build_status` content test that exercises
the *realistic* degraded state (membership present -> missing member renders as an
`Offline` row), mirroring the VM Phase 3 contract at unit speed and making the dead
mocks load-bearing.

## Approach

Delete `cmd_status_degraded_ok` and add one `build_status`-level test in its place
(`cli/src/status.rs`, same test module). Production code is **not** touched; no
fixtures change -- every helper already exists.

### New test (sketch)

Name e.g. `build_status_degraded_3disk_1missing_classifies_offline_member`. Open with
the required `// Intent / Why it exists / Scenario` preamble.

Setup -- reuse the membership-rendering wiring from
`build_status_present_member_renders_by_id_and_operator_name` (it already pairs this
exact verbose runner with `status_membership_3disk()`), swapping the btrfs show to the
degraded fixture:

```rust
let runner = status_runner_healthy_3disk_verbose(status_runner_healthy_3disk_base())
    .with_output(
        CmdRequest::BtrfsFilesystemShow { mount_point: status_mp() },
        status_btrfs_show_3disk_1missing(),            // override healthy -> degraded
    )
    // probe_config_disk (by-id luksUUID comes from `verbose`) + the verbose-detail
    // luksDump / closed-mapper probes for the membership-named disks:
    .with_luks_dump_text_luks2_for(&[
        "/dev/disk/by-id/disk1", "/dev/disk/by-id/disk2", "/dev/disk/by-id/disk3",
    ])
    .with_mappers_closed(&["braid-toshiba1", "braid-toshiba2", "braid-toshiba3"]);
let fs = status_fs_three_disk();
let config = status_config();
let (_tmp, paths) = isolated_paths();
membership::save_membership(&status_membership_3disk(), &paths).unwrap();

let built = build_status(
    &runner, &fs, &config, &paths,
    crate::test_fixtures::mock_virtio_backing_path_resolver(),
).unwrap();
```

Why `verbose`: `status_runner_healthy_3disk_base` seeds only the live `/dev/vd*` UUID
probes; the by-id `luksUUID` probes that `probe_config_disk` needs come from
`status_runner_healthy_3disk_verbose` (which also adds device-stats + lsblk). The
`luksDump` + closed `braid-toshiba*` mapper-status mocks feed the verbose detail
rendering of the membership-named rows.

`status_membership_3disk()` carries UUIDs `1111.../2222.../3333...` and by-id paths
`disk1/2/3`, matching the runner's LUKS UUIDs. disk1/disk2 are live in the pool
(gated out via `membership_uuid_live`, rendered as present rows with operator names
toshiba1/toshiba2); disk3 is absent from btrfs, so the config-disk loop in
`build_disk_views` classifies it `PresentLuks` + UUID-match + not-in-pool ->
`DiskStatus::Offline` (config-disk arm of `cli/src/status.rs#build_disk_views`).

Assertions -- report-level, structure-insensitive (filter by status, don't index):

- `built.report.status == StatusCode::Degraded`
- `built.report.total_devices == Some(3)`
- `built.report.present_count == Some(2)`
- `built.report.missing_count == Some(1)`
- Exactly 2 rows with `status == DiskStatus::Present`, exactly 1 with
  `status == DiskStatus::Offline`.
- Pin the offline row's identity (find the sole `Offline` row by status, don't index):
  `name == "toshiba3"`, `by_id == "/dev/disk/by-id/disk3"`, `mapper == "braid-toshiba3"`,
  `devid == None`. This guards a regression that keeps the 2/1 counts but loses the
  persisted member identity VM Phase 3 pins. (`mapper` is `mapper_name("toshiba3")` =
  `braid-toshiba3`; see `cli/src/config.rs#mapper_name`. `luks_uuid` is blank for the
  `Offline` arm.)

**Do not** assert `missing_devids` here: with the `3disk_1missing` (total-minus-listed)
fixture btrfs never names the devid, so `missing_devids` is empty even though
`missing_count == 1`. That union contract is owned by
`build_status_missing_devids_unions_btrfs_missing_and_null_underlying`; duplicating it
here would couple to an unrelated branch.

Keep this a report-level (assembly) test -- no human-string assertions. Degraded
rendering is already covered by `status_human_degraded` / `status_json_degraded`, and
the VM Phase 3 holds the end-to-end string contract.

### Smoke siblings -- leave as-is

`cmd_status_not_mounted_ok`, `cmd_status_healthy_ok`, `cmd_status_healthy_json_ok`
stay untouched. They are the only end-to-end coverage of the `cmd_status`
print/JSON dispatch wrapper, their assembled scenarios are content-covered by
existing `build_status` tests, and the degraded scenario loses no `cmd_status`-level
coverage by conversion (degraded human/JSON rendering is covered by
`status_human_degraded` / `status_json_degraded`). Only the degraded test was
misleading (rich mock, no assertions); fixing it dissolves the finding without churn.

## Critical files

- `cli/src/status.rs` -- replace the `cli/src/status.rs#cmd_status_degraded_ok` test
  with the new `build_status_degraded_*` test. No other edits.

## Reuse (all exist; no new helpers)

- `cli/src/test_fixtures/status.rs#status_runner_healthy_3disk_base`
- `cli/src/test_fixtures/status.rs#status_runner_healthy_3disk_verbose` (supplies the by-id `luksUUID` probes)
- `cli/src/test_fixtures/status.rs#status_btrfs_show_3disk_1missing`
- `cli/src/test_fixtures/status.rs#status_membership_3disk`
- `cli/src/test_fixtures/status.rs#status_fs_three_disk`, `#status_config`
- `cli/src/test_fixtures/doctor.rs#isolated_paths`
- `cli/src/config.rs#mapper_name` (offline-row mapper derivation -> `braid-toshiba3`)
- Offline classification -- config-disk arm of `cli/src/status.rs#build_disk_views`
- Wiring template -- `cli/src/status.rs#build_status_present_member_renders_by_id_and_operator_name`

## Verification

- TDD: write the assertions first, run the test, and confirm it **fails for the right
  reason** -- temporarily force the `StatusCode::Degraded` classification in
  `cli/src/status.rs#build_status` to `Intact`, or perturb a count, and watch the new
  assertions trip (not a bare `is_ok`).
- `just test-rust` -- new test green; the 3 smoke siblings and the neighbouring
  `build_status_*` degraded tests still pass.
- The VM test `tests/cli/braid-status-rust.py` Phase 3 remains the end-to-end
  authority; this test is its fast, deterministic mirror.

## Implementation notes

- Beyond the test swap, removed two now-dead test-module imports
  (`status_btrfs_device_stats_3disk`, `status_btrfs_df_raid1`) from
  `cli/src/status.rs`. The deleted `cmd_status_degraded_ok` was their only
  consumer in that module (the new test gets those fixtures transitively via
  `status_runner_healthy_3disk_base`), so the compiler flagged them unused. This
  is the only edit outside the test body the plan called for; production code is
  untouched.
