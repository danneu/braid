# Plan: pin offline-member SMART self-test by-id device selection

## Context

`check_smart_selftests` (`cli/src/doctor.rs#check_smart_selftests`) picks the
device it probes per member with:

```rust
let query_device = live
    .and_then(|pool| pool.underlying_for_uuid(uuid))   // present -> live backing path
    .unwrap_or(by_id);                                  // offline -> persisted by-id
```

For an **offline** member (in `pool.json` membership but absent from the mounted
`PoolState.devices`), `underlying_for_uuid` returns `None`, so the probe correctly
falls back to the stable by-id handle. This is the ADR-024 contract
(`docs/design/decisions/024-luks-uuid-identity.md`): present members read the live
backing path; an offline member reads its persisted by-id handle, "so the two SMART
surfaces cannot disagree under by-id drift."

**The gap:** that offline -> by-id arm is unpinned for doctor. The two existing
live-path tests (`check_smart_selftest_present_member_queries_live_underlying`,
`..._warn_hint_uses_by_id`) only cover an *assembled* member. The multi-drive tests
(`..._emits_one_result_per_drive`, `..._mixed_statuses_one_per_drive`) run with a
plain `MockRunner` and **no live pool at all**, so every member falls to by-id via
the pool-offline branch -- they never exercise per-member discrimination inside a
*mounted* pool. A refactor that indexed live devices by name or devid and wrongly
matched an offline member to a *sibling's* live `underlying` would route the SMART
probe to the wrong physical device, and every current test would still pass.

This is the symmetric counterpart to the assembled-member live-path pins and to
`check_declared_disks_present_member_probes_by_id_not_live`. The TUI side already
pins **both** arms in one test (`cli/src/tui/browse/state.rs#smartctl_picker_resolves_present_member_to_live_path`);
this closes the one unfilled corner of the matrix on the doctor side.

**Outcome:** one new Rust unit test in `cli/src/doctor.rs`. No production code
changes -- the behavior is already correct; this pins it against regression. A
"unify device selection" refactor is explicitly *not* wanted (ADR-024 keeps doctor
and TUI as two independently-pinned surfaces, and `check_declared_disks` deliberately
diverges by always using by-id).

## Change

Add one test next to the existing SMART device-selection pins (after
`cli/src/doctor.rs#check_smart_selftest_present_member_warn_hint_uses_by_id`).

Name (mirrors the `..._by_id_not_live` sibling):
`check_smart_selftest_offline_member_queries_by_id_not_live`.

### Construction (reuses existing helpers only)

- **Membership** (`save_doctor_membership`): two members --
  `(1, "disk1", "/dev/disk/by-id/disk1", Some(Devid::new(1)))` and
  `(2, "disk2", "/dev/disk/by-id/disk2", Some(Devid::new(2)))`.
- **Live pool** (`pool_state_runner`): register **only disk2** as assembled --
  `pool_state_runner(vec![("braid-disk2", 2, "/dev/vdc", test_uuid(2))], &[])`.
  `probe_pool` enumerates members from the `BtrfsFilesystemShow` mock, so
  `PoolState.devices == [disk2 @ /dev/vdc]`; disk1 is absent -> offline. (The
  `mounted_btrfs_only()` mountinfo source `braid-disk1` is incidental -- it only
  confirms a btrfs mount at `/mnt/storage`; it does not add disk1 to the live set.)
- **smartctl mocks** (`smartctl_selftest_json` + `.with_output`), with *distinct*
  fixtures so a mis-route flips the result:
  - disk1 offline by-id `"/dev/disk/by-id/disk1"` -> `smartctl-selftest-ata-recent-pass.json`, exit 0 (Ok, "passed ~2 days ago").
  - disk2 live `"/dev/vdc"` -> `smartctl-selftest-ata-stale.json`, exit 0 (Warn, "~125 days").
  - **Do not** register a by-id mock for disk2: its by-id path stays unregistered,
    so a wrong by-id probe for the live member degrades to `MissingMock` -> `Skip`.
- **fs / ctx**: `DoctorMockFs::mounted_btrfs_only()` and
  `DoctorContext::for_test_parsed_with_fs(&runner, &fs, &paths, valid_config_json())`
  (identical to the assembled-member test). No `.with_block_device` needed -- the
  SMART check does not gate on `ctx.fs` block-device classification.
- Call `check_smart_selftests(&mut ctx)`; `drop(dir)` after.

### Assertions (counterfactually anchored)

Primary pin -- collect the SMART probe devices **in issue order** from the recorded
request log (`cli/src/cmd.rs#MockRunner::requests`), filter for
`CmdRequest::SmartctlSelftestLogJson { device }`, and assert the **full list** for
exact equality (not `contains`):

```rust
let smart_devices: Vec<String> = runner
    .requests()
    .into_iter()
    .filter_map(|r| match r {
        CmdRequest::SmartctlSelftestLogJson { device } => Some(device),
        _ => None,
    })
    .collect();
assert_eq!(
    smart_devices,
    vec![
        "/dev/disk/by-id/disk1".to_owned(), // offline member -> by-id
        "/dev/vdc".to_owned(),              // live member -> underlying
    ],
);
```

`check_smart_selftests` issues exactly one `SmartctlSelftestLogJson` per member in
`membership.iter_by_name()` order (sorted by `DiskName`: disk1, disk2 --
`cli/src/membership.rs#iter_by_name`), and `requests()` returns the full call-order
log, so the filtered list is deterministic. Exact equality pins **count, order, and
values** in one assertion -- it catches a mis-routed offline probe (disk1 ->
`/dev/vdc`), the live member wrongly probed by-id (`/dev/disk/by-id/disk2` would
appear), **and** any extra or duplicate probe. SMART probes spin up the drive, so
the exact probe set is an observable side effect worth pinning whole, not just
testing for membership. This is the strongest, structure-insensitive pin and an
exact match for the finding's request.

Corroborating row assertions (reuse `by_subject`):

- `by_subject(&results, "disk1")`: `status == Ok`, message contains
  `"passed ~2 days ago"` (proves the recent-pass *by-id* fixture was read; a
  mis-route to `/dev/vdc` would make it Warn "~125 days").
- `by_subject(&results, "disk2")`: `status == Warn` (proves the stale *live*
  fixture was read). Do **not** assert disk2's message excludes by-id -- its Warn
  hint legitimately cites `/dev/disk/by-id/disk2` while the probe used the live
  node; the request-log assertion is what pins probe vs hint.

### Test preamble (per `docs/dev/testing.md`)

- **Intent:** an offline member (present in membership, absent from the mounted
  pool) is SMART-probed via its persisted by-id handle, while an assembled sibling
  in the same mounted pool is probed via its live backing path.
- **Why it exists:** the existing live-path tests cover only the assembled arm, and
  the multi-drive tests run with no live pool (by-id for the wrong reason). Nothing
  pinned the offline arm inside a *mounted* pool, so a refactor indexing live
  devices by name/devid could route an offline member's probe to a sibling's live
  node undetected. Symmetric counterpart to the assembled live-path pins and to
  `check_declared_disks_present_member_probes_by_id_not_live`; serves the ADR-024
  invariant that doctor and the TUI SMART surfaces cannot disagree under by-id drift.
- **Scenario:** a two-disk pool where disk2 is assembled at `/dev/vdc` but disk1 is
  present-but-unassembled; doctor must read disk1's stable by-id handle, not disk2's
  live node.

## Files

- `cli/src/doctor.rs` -- add the single `#[test] fn`, no production changes.

## Verification

- Single test (fast inner loop):
  `cargo test --manifest-path cli/Cargo.toml --lib check_smart_selftest_offline_member_queries_by_id_not_live`
- Full Rust suite before commit: `just test-rust`.
- Sanity-check the anchor: temporarily change the selection to
  `.unwrap_or(by_id)` -> a forced live path for all members (e.g. hardcode
  `/dev/vdc`) and confirm the new test fails on the missing `"/dev/disk/by-id/disk1"`
  request; revert.
