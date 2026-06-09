# Plan: pin the SMART self-test warn-hint by-id contract under a live pool

## Context

`doctor`'s per-drive `smart_self_test` check deliberately uses **two different
device handles** for a present pool member:

- **Probe device** (`query_device`): the member's *live* backing path
  (`/dev/sdX`) when it is assembled into the mounted pool, so a drifted by-id
  path can't make doctor report stale/missing SMART data for a healthy disk.
  Built at `cli/src/doctor.rs#check_smart_selftests` as
  `underlying_for_uuid(uuid).unwrap_or(by_id)`.
- **Hint device** (`by_id`): the *persisted by-id* path, passed as a separate
  argument to `summarize_smart_selftest` and embedded in the operator-facing
  remediation hint (`run: smartctl -t short <by_id> ...`). By-id is used here on
  purpose because the command the operator runs *later* must survive reboots and
  controller reordering.

This split is a documented contract: `docs/commands/doctor.md` ("The hint uses
the stable by-id path: braid's own diagnostic read prefers the member's live
backing device, but a `smartctl -t short` you run later should use by-id") and
ADR-024 `#benefits`.

**The gap:** no test pins the split on a branch that *emits the hint*. The one
live-pool test (`check_smart_selftest_present_member_queries_live_underlying`)
asserts only the `Ok` branch, which prints no hint. Every warn-hint test
(`check_smart_selftest_message_contains_by_id_path`,
`check_smart_selftest_stale_warns_with_age`) runs through
`single_selftest_fixture_results`, which builds a bare `MockRunner` with **no
pool state** -- so `query_device` falls back to `by_id` and the two handles are
identical. A refactor that collapsed the arguments (e.g. passing `query_device`
into `summarize_smart_selftest` and reusing it for the hint) would print the
unstable live `/dev/sdX` node in the warn hint while **every existing test still
passed**, silently violating the ADR-024 by-id-survives-reboots contract.

The split was introduced in `8593c9fe` ("query present hardware through live
paths"), which added only the `Ok`-branch live-pool test. This plan fills the
warn-arm coverage with a single regression test. No production code changes.

## The fix

Add one Rust unit test to the `tests` module in `cli/src/doctor.rs`, placed
immediately after `check_smart_selftest_present_member_queries_live_underlying`
(currently ends ~line 2708) so the two live-pool tests sit together. It is the
deliberate **dual** of that test: same membership + `pool_state_runner` setup,
inverse fixture polarity (stale on the *live* path -> Warn; recent-pass on the
*by-id* path), so it jointly pins both halves of the contract -- "probe follows
the live path even when by-id is the healthy side" *and* "the hint stays by-id."

```rust
// Intent: a present member's stale-self-test Warn hint cites the persisted
//   by-id path even though the probe read the live backing device.
// Why it exists: probe device (live /dev/sdX) and hint device (by-id) are
//   deliberately separate args; a refactor collapsing them would print an
//   unstable /dev/sdX in the "smartctl -t short ..." hint, violating the
//   ADR-024 by-id-survives-reboots contract. The only prior live-pool test
//   asserts the Ok branch, which emits no hint -- so nothing pinned the warn
//   arm where probe device != hint device.
// Scenario: disk1 is present at /dev/vdb whose live self-test is stale (warns),
//   while the persisted by-id mock would have passed.
#[test]
fn check_smart_selftest_present_member_warn_hint_uses_by_id() {
    let (dir, paths) = isolated_paths();
    save_doctor_membership(&paths, &[(1, "disk1", "/dev/disk/by-id/disk1", Some(1))]);
    let (live_req, live_out) =
        smartctl_selftest_json("/dev/vdb", "smartctl-selftest-ata-stale.json", 0);
    let (by_id_req, by_id_out) = smartctl_selftest_json(
        "/dev/disk/by-id/disk1",
        "smartctl-selftest-ata-recent-pass.json",
        0,
    );
    let runner = pool_state_runner(vec![("braid-disk1", 1, "/dev/vdb", test_uuid(1))], &[])
        .with_output(live_req, live_out)
        .with_output(by_id_req, by_id_out);
    let fs = DoctorMockFs::mounted_btrfs_only();
    let mut ctx =
        DoctorContext::for_test_parsed_with_fs(&runner, &fs, &paths, valid_config_json());

    let results = check_smart_selftests(&mut ctx);
    drop(dir);

    let r = only_result(&results);
    assert_eq!(r.status, CheckStatus::Warn);
    // Probe read the live stale fixture, not the by-id recent-pass.
    assert!(r.message.contains("~125 days"), "{}", r.message);
    // Hint cites the stable by-id path and never leaks the live /dev/vdb node.
    assert!(r.message.contains("/dev/disk/by-id/disk1"), "{}", r.message);
    assert!(!r.message.contains("/dev/vdb"), "{}", r.message);
}
```

### Why these specific assertions

- `status == Warn` + `contains("~125 days")` -- proves the probe read the
  **live** stale fixture (`power_on 5000` / `last pass lifetime 2000` -> 3000 h
  -> 125 days > 90 d threshold). If a regression made the probe read by-id
  instead, it would get the recent-pass fixture and the status would be `Ok`,
  failing here.
- `!contains("/dev/vdb")` -- the **load-bearing** assertion. This is the one
  that fails under the argument-collapse refactor; the positive by-id check
  alone is already covered by `check_smart_selftest_message_contains_by_id_path`.
- `contains("/dev/disk/by-id/disk1")` -- confirms the hint is present and
  correctly built from by-id.

## Reused helpers (no new test infrastructure)

All already imported in the `doctor.rs` `tests` module; the test is pure
composition of existing fixtures:

- `pool_state_runner`, `smartctl_selftest_json`, `test_uuid` --
  `cli/src/test_fixtures/doctor.rs` / `shared.rs`
- `save_doctor_membership`, `only_result` -- local helpers in
  `cli/src/doctor.rs` tests module
- `DoctorMockFs::mounted_btrfs_only`, `DoctorContext::for_test_parsed_with_fs`,
  `valid_config_json` -- existing doctor test harness
- Fixtures `smartctl-selftest-ata-stale.json` and
  `smartctl-selftest-ata-recent-pass.json` already exist under
  `cli/tests/fixtures/nixos-26.05/`

## Scope decisions (considered and excluded)

- **Only the warn arm, not the fail arm.** `summarize_smart_selftest` embeds
  `by_id` in three branches (the two warn hints at `#summarize_smart_selftest`
  plus the rare active-errors-without-entry fail fallback). All three read the
  **same** `by_id` parameter, so one warn-arm test catches the realistic
  "collapse the two arguments" refactor. A separate fail-arm test would only
  guard a branch-local edit to a defensive parse-inconsistency path -- marginal
  value, and it edges toward structure-sensitive testing. Excluded.
- **No type-level enforcement (e.g. a `QueryDevice` newtype distinct from
  `ByIdPath`).** Tempting, but `query_device` is `underlying_for_uuid(uuid)
  .unwrap_or(by_id)` -- it is *legitimately* the by-id `&str` on the fallback
  path, so the two cannot be cleanly separated into non-interchangeable types
  without fighting the deliberate `unwrap_or` design. Overkill for a Low-severity
  test gap. Excluded.
- **No production code change.** The split is correct as written; only its
  test coverage is missing.

## Verification

1. Targeted run (fast): from `cli/`, run
   `cargo test --lib check_smart_selftest_present_member` -- runs both the
   existing live-path test and the new warn-hint test.
2. Full Rust suite: `just test-rust`
   (`cargo test --lib --bin braid --test golden_nixos_26_05 --test tty_guard
   --test confirm_yes`).
3. **Confirm the test actually pins the contract (red/green check).** Before
   trusting it, temporarily edit `check_smart_selftests` to pass `query_device`
   in place of `by_id` to `summarize_smart_selftest`, confirm the new test fails
   on the `!contains("/dev/vdb")` assertion, then revert. This proves the test
   guards the exact divergence it exists to prevent (do not commit the temporary
   edit).
