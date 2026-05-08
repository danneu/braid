# Plan: Migrate `cli/src/mount.rs` test scaffolding to a shared `test_fixtures::mount` module

**Status: Draft**

## Context

`cli/src/mount.rs` is 4107 lines, of which lines 828-4107 (the `#[cfg(test)] mod tests` block) hold 48 tests plus ~270 lines of inline scaffolding: a local `MockFs` (mount.rs:840), `ok_raw`/`err_raw` (870/879), `open_and_mount_for_test` (898), `test_passphrase`/`test_config` (924/930), `two_disk_membership`/`three_disk_membership` (934/948), `luks_uuid_ok` (963), `arbitrary_fallback` (2572), `test_passphrase_fail`/`is_luks_ok`/`is_luks_fail`/`luks_dump_text_ok`/`luks_dump_text_fail` (2723-2783), `base_two_disk_runner` (2787), `two_disk_fs` (2824), `NoopSleeper` (2831), `direct_two_disk_plan` (2837), `direct_two_disk_fs_with_mappers` (2855), `direct_two_disk_open_runner` (2864).

Twelve of the 48 tests redundantly inline the same canonical 2-disk-closed preflight that `base_two_disk_runner()` already implements (each ~50 lines: `MountpointCheck` + UUID×2 + LuksDumpText×2 + mappers closed + `CryptsetupTestPassphrase` ok×2). Promoting `base_two_disk_runner()` to shared scope and refactoring those tests collapses each setup to roughly 6 lines. The remaining tests fall into three groups that benefit less from a topology installer:

- pure-formatter tests (`format_degraded_refused_*`, `render_probe_events_*`, `probe_event_to_preview_note_*`, `explain_open_failure_*`) — already factored, no runner/fs needed.
- ProbeFailed-uncertainty tests (`unlock_passphrase_open_exit2_probe_failed_does_not_blame_invariant` mount.rs:3611) that deliberately omit a probe so `MockRunner::CmdError::MissingMock` collapses to `LuksHeaderState::ProbeFailed`. Any topology installer that auto-seeds `CryptsetupIsLuks`/`CryptsetupLuksDumpText` for every disk silently breaks them.
- cleanup-ordering tests (mount.rs:2907-3434) that already use `direct_two_disk_open_runner` and pin `runner.requests()` ordering and zero-side-effect invariants.

Outcome: ship `cli/src/test_fixtures/mount.rs` as a flat collection of helpers (modeled on `test_fixtures/doctor.rs`'s no-`*Pool`-no-`*ParamsBuilder` shape, since `execute_mount_only`, `execute_unlock_and_mount`, `plan_open_pool` have no params struct), reuse `shared::MockFs::unmounted` as the filesystem (verified safe -- mount/probe/luks production code never calls `fs.read_to_string` or `fs.is_block_device` on the planning path; only `fs.exists`), and migrate tests in five small sub-commits keeping `just test-rust` green at each boundary. Don't ship a topology handler installer -- the ProbeFailed-uncertainty constraint and mapper-state diversity (closed / open / partially-open / wrong-uuid-open) make a single fixture handler too narrow or too broad. This mirrors `recover.rs`'s deliberate decision (test_fixtures/recover.rs:1-14).

This is unreleased software (AGENTS.md "No backwards compatibility"), so we delete old scaffolding rather than deprecate it.

## Recommended approach

### A. New module `cli/src/test_fixtures/mount.rs`

Gated `#[cfg(test)]`; registered in `cli/src/test_fixtures.rs` as a private submodule (`mod mount;`) with `pub(crate) use mount::{...}` re-exports through the facade -- matching the existing pattern for `add`, `doctor`, `recover`, `remove`, `remove_missing`, `replace`, `shared` (test_fixtures.rs:40-67). Sibling test code imports via the facade only, e.g. `use crate::test_fixtures::{base_two_disk_runner, mount_fs, MOUNT_TEST_PASSPHRASE_BYTES};` -- never `crate::test_fixtures::mount::{...}`, since `mod mount;` is private to `test_fixtures.rs`. This convention is established at `cli/src/replace.rs:2549`, `cli/src/recover.rs:3275`, `cli/src/doctor.rs:1034`, `cli/src/remove.rs:688`, `cli/src/remove_missing.rs:585`. All items inside the new module are `pub(crate)` and test-only -- no `///` doc-comment requirement (AGENTS.md). Module-level doc comment explains why this scope ships flat helpers (no `*Pool` topology installer, no `*ParamsBuilder`) -- the ProbeFailed-uncertainty constraint plus the no-params-struct entry points.

Items in the module:

```rust
// Filesystem
pub(crate) fn mount_fs(paths: &[&str]) -> shared::MockFs;
    // Thin wrapper: shared::MockFs::unmounted(paths.iter().map(...).collect()).
    // mount.rs's plan_open_pool_inner uses runner.run(MountpointCheck) for the
    // mountedness probe, never fs.read_to_string("/proc/self/mountinfo"), so the
    // shared mock's mountinfo body is unread for these tests. Verified by grep --
    // see Verification.

// Sleeper
pub(crate) struct NoopSleeper;
impl crate::progress::Sleeper for NoopSleeper { /* no-op */ }

// Output factories (RawCommandOutput primitives)
pub(crate) fn ok_raw(cmd: &str) -> RawCommandOutput;
pub(crate) fn err_raw(cmd: &str, exit_code: i32, stderr: &str) -> RawCommandOutput;

// (CmdRequest, RawCommandOutput) pair factories for chaining
pub(crate) fn luks_uuid_ok(device: &str, uuid: &str) -> (CmdRequest, RawCommandOutput);
pub(crate) fn test_passphrase_fail(device: &str) -> (CmdRequest, RawCommandOutput);
pub(crate) fn is_luks_ok(device: &str) -> (CmdRequest, RawCommandOutput);
pub(crate) fn is_luks_fail(device: &str) -> (CmdRequest, RawCommandOutput);
pub(crate) fn luks_dump_text_ok(device: &str) -> (CmdRequest, RawCommandOutput);
pub(crate) fn luks_dump_text_fail(device: &str) -> (CmdRequest, RawCommandOutput);

// Membership / config / credential constructors
pub(crate) const MOUNT_TEST_PASSPHRASE_BYTES: &[u8] = b"testpass";
pub(crate) fn test_config() -> Config;                  // /mnt/storage
pub(crate) fn test_passphrase() -> OpenCredential;      // Passphrase("testpass")
pub(crate) fn two_disk_membership() -> PoolMembership;
pub(crate) fn three_disk_membership() -> PoolMembership;
pub(crate) fn arbitrary_fallback() -> MountError;       // for explain_open_failure tests

// Composite preflight runners (the high-leverage helpers)
pub(crate) fn base_two_disk_runner() -> MockRunner;
    // Canonical 2-disk-closed preflight: MountpointCheck not-mounted, UUID×2,
    // LuksDumpText luks2×2, mappers_closed×2, test_passphrase ok stdin×2.
    // SAFE for ProbeFailed-uncertainty tests: it does NOT auto-seed
    // CryptsetupIsLuks. Tests that need different verify outcomes for a disk
    // chain `.with_output_stdin(test_passphrase_fail(...))` on top -- HashMap
    // overwrite semantics in MockRunner::with_output_stdin (cmd.rs:1010-1012)
    // make the override win.

// Direct execute_*-style fixtures used by the cleanup-ordering family
pub(crate) fn direct_two_disk_plan() -> OpenPlan;
pub(crate) fn direct_two_disk_fs_with_mappers() -> shared::MockFs;
pub(crate) fn direct_two_disk_open_runner() -> MockRunner;

// Test harness around plan + execute_*
pub(crate) fn open_and_mount_for_test<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R, fs: &F, config: &Config, membership: &PoolMembership,
    credential: Option<OpenCredential>, allow_degraded: bool, command_hint: &str,
) -> Result<bool, MountError>;
```

**What does NOT go in this module** (intentional omissions):

- No `MountTopology` / `MountPool` handler installer. ProbeFailed tests at mount.rs:3611, 3784 deliberately omit `CryptsetupIsLuks`/`CryptsetupLuksDumpText` for a specific disk to assert "diagnosis could not be completed" wording. A broad `with_handler` would resolve those probes and silently break the tests.
- No `MountParamsBuilder`. The functions under test (`execute_mount_only`, `execute_unlock_and_mount`, `plan_open_pool`, `close_opened_mappers`, `format_degraded_refused`, `render_probe_events`, `explain_open_failure`) take positional args, not a params struct. There is no `MountParams` to build.
- No `PoolFixture`. mount.rs::tests construct `PoolMembership` in-memory and call `Config::new(MountPoint(...))` -- never load membership/config from disk via `StatePaths`. The shared `PoolFixture::two_disk_healthy` writes `pool.json` to a tempdir, which would be wasted work here.
- No new Filesystem trait impl. `shared::MockFs::unmounted` already exists at test_fixtures/shared.rs:60 (currently `#[allow(dead_code)]`) and matches mount tests' needs once we confirm production code never reads mountinfo via the trait (it doesn't).

### B. Migration ordering principle

Move scaffolding once, then collapse inline preflights one family at a time. Hard cases first, bulk second:

- (a) **Cleanup-ordering tests** are the highest-risk family because they assert exact `runner.requests()` ordering (`assert_eq!(close_positions, vec![(forget_pos+1, "braid-disk1"), (forget_pos+2, "braid-disk2")])` at mount.rs:3358) and load-bearing zero-side-effect invariants (`captured == ""` at mount.rs:3234, `!runner.requests().iter().any(forget|close)` at mount.rs:3236-3241). Their migration is import-only -- no behavior change risk. Land first.
- (b) **2-disk-closed command-level tests** are the actual collapse win. Refactor 9 inline preflights to `base_two_disk_runner()` + ~4 trailing chains.
- (c) **3-disk and open-mappers tests** (~5 tests) don't share a base runner. Inline using moved leaf helpers; don't invent one-off `base_three_disk_*` helpers for tests that wouldn't share them.
- (d) **Unlock-error-routing tests** already use `base_two_disk_runner` locally; migration is a `use` swap.
- (e) **Pure formatter / explain-open-failure tests** stay as-is except for moving `arbitrary_fallback` and any moved leaf-helper imports. No structural change.

### C. Migration table

| Sub-commit | Action | Validates |
|---|---|---|
| 1 | Land `cli/src/test_fixtures/mount.rs` with the items in §A. Register `mod mount;` (private) + `#[allow(unused_imports)] pub(crate) use mount::{...}` facade re-exports in `test_fixtures.rs` (matching the existing `doctor`/`recover`/`remove`/`remove_missing`/`shared` groups at test_fixtures.rs:48-67 -- the `unused_imports` allow is necessary because the consumer migration spans sub-commits 2-5; without it sub-commit 1 fails `cargo check --tests` on the unconsumed re-exports). Mark every item in the new module `#[allow(dead_code)]` since no consumers yet. Promote `shared::MockFs::unmounted` from `#[allow(dead_code)]` to live use. **Land the required cmd.rs regression test** `mock_runner_with_output_stdin_override_after_base_wins` in `cli/src/cmd.rs::tests` to pin the static-key overwrite contract that this plan relies on for per-test verify-outcome overrides on top of `base_two_disk_runner` (see §E for the test body and rationale). **Verify** that mount/probe/luks/credential_verify production code never calls `fs.read_to_string` or `fs.is_block_device` (grep cited in Verification). | Module compiles; new cmd.rs test passes (pins the override contract); `cargo check --tests` clean; `just test-rust` green. |
| 2 | **Cleanup family — import-only migration.** Replace local helper references with `use crate::test_fixtures::{...}` imports (facade re-exports, never `crate::test_fixtures::mount::{...}`) for the 8 tests at mount.rs:2907 (`unlock_failure_after_two_opens_closes_both_after_scoped_forget`), 2984 (`unlock_scan_failure_reports_opened_mappers_for_cleanup`), 3020 (`already_owned_execute_race_is_filtered_from_cleanup_set`), 3103 (`second_open_failure_preserves_error_and_cleans_first_open`), 3154 (`cleanup_busy_close_attempts_later_mappers_and_reports_guidance`), 3211 (`wrong_passphrase_zero_open_cleanup_is_noop`), 3250 (`keyfile_post_open_failure_reports_opened_mappers_for_cleanup`), 3374 (`cleanup_forget_failure_warns_and_still_closes_all_mappers`). These already call `direct_two_disk_open_runner`, `direct_two_disk_plan`, `direct_two_disk_fs_with_mappers`, `NoopSleeper`, `test_config`, `test_passphrase` -- the swap is `use` lines only. **Preserve byte-for-byte:** every ordering assertion (`close_positions`, `forget_pos < close_pos` at mount.rs:2975), every negative `runner.requests().iter().any(...)` assertion (mount.rs:3090, 3236), and every `captured == ""` / `captured.contains(...)` assertion. | `cargo test --manifest-path cli/Cargo.toml --lib mount::tests` green; `just test-rust` green. Ordering / negative-requests semantics preserved because handlers are NOT used here -- static-key dispatch is unchanged. |
| 3 | **2-disk-closed command-level tests — collapse to base_two_disk_runner.** Refactor 9 tests: mount.rs:1109 (`mount_two_disk_happy_path`), 1670 (`mount_passphrase_mismatch_names_disk`), 2246 (`mount_luks_uuid_mismatch_closed`), 2375 (`mount_non_auth_open_failure_propagates_passphrase`), 2470 (`mount_non_auth_open_failure_propagates_keyfile`), 2106 (`plan_open_pool_emits_events_before_degraded_refused`), plus the trivial 1083 (`mount_already_mounted_returns_false`), 1773 (`mount_no_unlockable_disks`), and the precondition guards 996 (`execute_unlock_and_mount_rejects_empty_plan`), 1047 (`execute_mount_only_rejects_non_empty_plan`). For the canonical-shape ones, the inline preflight (~50 lines) collapses to `base_two_disk_runner()` + chained `.with_output_stdin(LuksOpen ...)` + `.with_output(BtrfsDeviceScanAll, ...)` + `.with_output(Mount/MountWithOptions, ...)`. For tests that need to override a per-disk verify outcome (e.g. `mount_passphrase_mismatch_names_disk` flips disk2 to fail-exit-2), chain the override `.with_output_stdin(test_passphrase_fail("/dev/disk/by-id/virtio-disk2").0, MOUNT_TEST_PASSPHRASE_BYTES.to_vec(), test_passphrase_fail(...).1)` -- HashMap insert overwrites the static-key value seeded by the base, verified via `cmd.rs:1010-1012`. **Preserve byte-for-byte:** every `// Intent / Why it exists / Scenario` preamble, every load-bearing substring assertion ("device not found", "single-passphrase invariant", "wrong passphrase (rejected by disk1)", "no unlockable disks", "execute_unlock_and_mount called with empty plan.to_unlock", "execute_mount_only called with non-empty plan.to_unlock"), every `MountError` discriminant match. | Per-test `cargo test --manifest-path cli/Cargo.toml --lib mount::tests::<name>`; then full `mount::tests`; then `just test-rust`. |
| 4 | **3-disk and open-mappers tests — leaf-only migration.** Migrate 6 tests that don't share a base preflight: mount.rs:1199 (`mount_degraded_with_flag` -- 3-disk closed, disk3 absent), 1290 (`mount_degraded_refused` -- 3-disk closed, disk3 absent, no mount mock), 1804 (`mount_skip_already_open` -- 2-disk open mappers), 1880 (`plan_open_pool_degraded_first_absent_picks_open_mapper` -- 3-disk, disk1 absent, disk2/3 open), 2177 (`mount_degraded_first_absent_all_open_uses_open_mapper` -- same shape), 2307 (`mount_luks_uuid_mismatch_already_open` -- 2-disk, disk1 mapper open). For each, swap local helper references with `use crate::test_fixtures::{luks_uuid_ok, mount_fs, test_config, two_disk_membership, three_disk_membership, ...};` (facade re-exports, never the private `mount` submodule path); do NOT invent a `base_three_disk_runner` or `base_two_disk_open_runner` -- inline composition via moved leaf helpers is clearer for tests that wouldn't share a fixture. **Preserve byte-for-byte:** the "no mount mock — should never reach mount" comment and missing Mount mock at mount.rs:1350; the exact `ProbeEvent` event-vector assertions; the LuksUuid mismatch substrings ("aaaaaaaa", "ffffffff"); the `MountWithOptions` arg-checking. | Per-test runs, then `mount::tests`, then `just test-rust`. |
| 5 | **Unlock-error-routing family — import-only migration.** Migrate 9 tests at mount.rs:3436 (`unlock_passphrase_verify_fails_unreadable_header_emits_guidance`), 3500 (`unlock_damaged_luks2_metadata_fails_at_gateway`), 3557 (`unlock_passphrase_verify_fails_ok_header_preserves_wrong_passphrase`), 3612 (`unlock_passphrase_open_exit2_probe_failed_does_not_blame_invariant`), 3690 (`unlock_keyfile_open_exit_nonzero_unreadable_header_emits_guidance`), 3866 (`unlock_passphrase_verify_exit_5_ok_header_surfaces_open_failed`), 3925 (`unlock_passphrase_verify_exit_1_unreadable_header_emits_guidance`), 3984 (`unlock_keyfile_verify_exit_5_ok_header_surfaces_open_failed`), 4049 (`unlock_keyfile_verify_exit_1_unreadable_header_emits_guidance`). These already use `base_two_disk_runner` + `is_luks_*` + `luks_dump_text_*` + `test_passphrase_fail` locally; migration is `use` lines only. The ProbeFailed-uncertainty test at line 3612 deliberately omits `CryptsetupIsLuks` for disk2; verify the `base_two_disk_runner` we promote does NOT seed it (it doesn't -- mount.rs:2787 confirms). Also migrate the 5 `explain_open_failure_*` pure-helper tests at mount.rs:2587, 2630, 2675, 2698, 3784 to use the moved `arbitrary_fallback`. The 9 `format_degraded_refused_*` tests (1387, 1433, 1488, 1524, 1553, 1578, 1597, 1617, 1637) and 2 probe-event-render tests (1973, 2031) need no migration -- they touch only the formatter under test. | Full `mount::tests`; `just test-rust`. |
| 6 | **Cleanup**: delete the now-unused locals in mount.rs::tests: `MockFs` (840-868), `ok_raw` (870), `err_raw` (879), `open_and_mount_for_test` (898), `test_passphrase` (924), `test_config` (930), `two_disk_membership` (934), `three_disk_membership` (948), `luks_uuid_ok` (963), `arbitrary_fallback` (2572), `test_passphrase_fail` (2723), `is_luks_fail` (2736), `is_luks_ok` (2749), `luks_dump_text_ok` (2758), `luks_dump_text_fail` (2772), `base_two_disk_runner` (2787), `two_disk_fs` (2824), `NoopSleeper` (2831), `direct_two_disk_plan` (2837), `direct_two_disk_fs_with_mappers` (2855), `direct_two_disk_open_runner` (2864). Remove `#[allow(dead_code)]` annotations on `test_fixtures::mount` items now that every helper has a consumer. Update the `mod tests` header `use` lines. Confirm `cargo check --manifest-path cli/Cargo.toml --tests` is clean. | No dangling references; full `just test-rust`. |

### E. Required `cmd.rs` regression test (lands in sub-commit 1)

The migration relies on `MockRunner::with_output_stdin` overwriting **both** the `outputs` map and the `stdin_expectations` map for the same key, so per-test verify-outcome overrides on top of `base_two_disk_runner()` work end-to-end. The pattern in production mount tests is exactly this: `mount.rs:3566-3567` chains `.with_output_stdin(tp_req, b"wrongpass".to_vec(), tp_out)` after `base_two_disk_runner()` already seeded `with_output_stdin(tp_req, b"testpass".to_vec(), ok)` -- the override changes BOTH the expected stdin bytes AND the resolved output. Today this is correct because `MockRunner::with_output_stdin` (cmd.rs:1004-1014) does `self.outputs.insert(key, output)` and `self.stdin_expectations.insert(key, expected_stdin)` -- both `HashMap::insert` calls overwrite. Existing `cmd::tests` (cmd.rs:1207-1505) cover handler dispatch, stdin mismatch (`mock_runner_run_with_stdin_panics_on_stdin_mismatch_unchanged` at cmd.rs:1270), header-backup side effects, and request log -- but **not** the duplicate-key overwrite contract for either map. Add this required test as part of sub-commit 1 to pin the contract that the migration premise depends on:

```rust
// Intent: A second `with_output_stdin` call for the same `CmdRequest` must
//   overwrite both the registered output AND the expected stdin bytes set
//   by the first call -- not append, not shadow.
// Why it exists: the mount-fixture migration (plans/wip/plan-the-next-test-
//   fixture-effervescent-kernighan.md) relies on this so per-test verify
//   overrides chained on top of `base_two_disk_runner()` flip both the
//   expected stdin bytes and the resolved output (e.g. mount.rs:3566
//   chains `.with_output_stdin(tp_req, b"wrongpass", tp_out)` after the
//   base seeded `b"testpass"`). If a future MockRunner refactor switches
//   `outputs` or `stdin_expectations` to a queue/Vec, that override
//   pattern silently regresses; this test fails the moment it does.
// Scenario: register two `with_output_stdin` calls with the same request
//   key but distinct stdin byte strings and distinct outputs; call
//   `run_with_stdin` with the SECOND call's stdin bytes and assert the
//   SECOND output is returned. Success requires both `outputs.insert` and
//   `stdin_expectations.insert` to have overwritten -- if outputs did not
//   overwrite the cmd would be "first"; if stdin_expectations did not
//   overwrite the call would panic on stdin mismatch.
#[test]
fn mock_runner_with_output_stdin_override_after_base_wins() {
    let req = CmdRequest::CryptsetupTestPassphrase {
        device: "/dev/vdb".into(),
    };
    let runner = MockRunner::default()
        .with_output_stdin(req.clone(), b"testpass".to_vec(), RawCommandOutput {
            cmd: "first".into(),
            stdout: String::new(),
            stderr: String::new(),
            exit_status: 0,
        })
        .with_output_stdin(req.clone(), b"wrongpass".to_vec(), RawCommandOutput {
            cmd: "second".into(),
            stdout: String::new(),
            stderr: "wrong passphrase".into(),
            exit_status: 2,
        });

    // Calling with the SECOND registration's bytes proves stdin_expectations
    // was overwritten (no panic) AND outputs was overwritten (cmd == "second").
    let out = runner
        .run_with_stdin(&req, b"wrongpass")
        .expect("override stdin should match the second expectation");
    assert_eq!(out.cmd, "second", "second with_output_stdin must overwrite first output");
    assert_eq!(out.exit_status, 2, "override exit status must win");
}
```

Co-locate this with the existing `mock_runner_run_with_stdin_panics_on_stdin_mismatch_unchanged` test (cmd.rs:1270) so the overwrite contract sits next to its sibling stdin-validation contract.

### Sample migration (sub-commit 3, mount.rs:1109)

Before (~80 lines):

```rust
#[test]
fn mount_two_disk_happy_path() {
    let config = test_config();
    let membership = two_disk_membership();
    let fs = MockFs::new(&[
        "/dev/disk/by-id/virtio-disk1",
        "/dev/disk/by-id/virtio-disk2",
    ]);

    let (uuid1_req, uuid1_out) = luks_uuid_ok("/dev/disk/by-id/virtio-disk1", "aaaaaaaa-...");
    let (uuid2_req, uuid2_out) = luks_uuid_ok("/dev/disk/by-id/virtio-disk2", "bbbbbbbb-...");

    let runner = MockRunner::default()
        .with_output(CmdRequest::MountpointCheck { path: MountPoint("/mnt/storage".to_owned()) }, err_raw("mountpoint", 1, ""))
        .with_output(uuid1_req, uuid1_out)
        .with_output(uuid2_req, uuid2_out)
        .with_luks_dump_text_luks2("/dev/disk/by-id/virtio-disk1")
        .with_luks_dump_text_luks2("/dev/disk/by-id/virtio-disk2")
        .with_mappers_closed(&["braid-disk1", "braid-disk2"])
        .with_output_stdin(CmdRequest::CryptsetupTestPassphrase { device: "/dev/disk/by-id/virtio-disk1".into() }, b"testpass".to_vec(), ok_raw("cryptsetup open --test-passphrase"))
        .with_output_stdin(CmdRequest::CryptsetupTestPassphrase { device: "/dev/disk/by-id/virtio-disk2".into() }, b"testpass".to_vec(), ok_raw("cryptsetup open --test-passphrase"))
        .with_output_stdin(CmdRequest::CryptsetupLuksOpen { device: "/dev/disk/by-id/virtio-disk1".into(), mapper: "braid-disk1".into() }, b"testpass".to_vec(), ok_raw("cryptsetup open"))
        .with_output_stdin(CmdRequest::CryptsetupLuksOpen { device: "/dev/disk/by-id/virtio-disk2".into(), mapper: "braid-disk2".into() }, b"testpass".to_vec(), ok_raw("cryptsetup open"))
        .with_output(CmdRequest::BtrfsDeviceScanAll, ok_raw("btrfs device scan"))
        .with_output(CmdRequest::Mount { device: "/dev/mapper/braid-disk1".into(), mount_point: MountPoint("/mnt/storage".to_owned()) }, ok_raw("mount"));

    let result = open_and_mount_for_test(&runner, &fs, &config, &membership, Some(test_passphrase()), false, "unlock");
    assert!(result.unwrap());
}
```

After (~25 lines):

```rust
#[test]
fn mount_two_disk_happy_path() {
    let config = test_config();
    let membership = two_disk_membership();
    let fs = mount_fs(&[
        "/dev/disk/by-id/virtio-disk1",
        "/dev/disk/by-id/virtio-disk2",
    ]);

    let runner = base_two_disk_runner()
        .with_output_stdin(
            CmdRequest::CryptsetupLuksOpen { device: "/dev/disk/by-id/virtio-disk1".into(), mapper: "braid-disk1".into() },
            MOUNT_TEST_PASSPHRASE_BYTES.to_vec(),
            ok_raw("cryptsetup open"),
        )
        .with_output_stdin(
            CmdRequest::CryptsetupLuksOpen { device: "/dev/disk/by-id/virtio-disk2".into(), mapper: "braid-disk2".into() },
            MOUNT_TEST_PASSPHRASE_BYTES.to_vec(),
            ok_raw("cryptsetup open"),
        )
        .with_output(CmdRequest::BtrfsDeviceScanAll, ok_raw("btrfs device scan"))
        .with_output(
            CmdRequest::Mount { device: "/dev/mapper/braid-disk1".into(), mount_point: MountPoint("/mnt/storage".to_owned()) },
            ok_raw("mount"),
        );

    let result = open_and_mount_for_test(&runner, &fs, &config, &membership, Some(test_passphrase()), false, "unlock");
    assert!(result.unwrap());
}
```

The `// Intent / Why / Scenario` preamble (which lives above this snippet at mount.rs:1101-1107) is preserved byte-for-byte.

## Critical files to modify

- `/Users/dan/Code/braid/cli/src/test_fixtures/mount.rs` -- NEW. Items per §A.
- `/Users/dan/Code/braid/cli/src/test_fixtures.rs` -- add `mod mount;` (private) and `#[allow(unused_imports)] pub(crate) use mount::{...}` facade re-exports for the items mount.rs::tests references. The `unused_imports` allow follows the existing pattern at lines 48-67 -- it is required because consumers land in later sub-commits (2-5) and `cargo check --tests` would otherwise fail on the unconsumed re-exports during the staggered rollout. Update the module-level doc-comment to mention the mount scope.
- `/Users/dan/Code/braid/cli/src/test_fixtures/shared.rs` -- drop `#[allow(dead_code)]` on `MockFs::unmounted` (line 60) once the mount fixture promotes it.
- `/Users/dan/Code/braid/cli/src/cmd.rs` -- add the required regression test `mock_runner_with_output_stdin_override_after_base_wins` to `cmd::tests` (see §E for the body). Pins the static-key overwrite contract on `with_output_stdin` (cmd.rs:1004-1014) that the per-test verify-outcome override pattern in sub-commit 3 depends on. No production change.
- `/Users/dan/Code/braid/cli/src/mount.rs` -- delete the inline scaffolding listed in sub-commit 6 (lines 840-921, 924-975, 2572-2574, 2723-2783, 2787-2905) and replace local references with `use crate::test_fixtures::{...}` facade imports (never `crate::test_fixtures::mount::{...}`, since `mod mount;` is private to `test_fixtures.rs`) per the table.

## Existing functions / utilities reused

- `shared::MockFs::unmounted` (test_fixtures/shared.rs:60) -- already implements `Filesystem`; the mount fixture wraps it via `mount_fs(paths: &[&str])` for ergonomic per-test calls.
- `cmd::MockRunner::with_output` / `with_output_stdin` / `with_mappers_closed` / `with_mapper_open` / `with_mapper_closed` / `with_luks_dump_text_luks2` (cmd.rs:988, 1004, 1126, 1138, 1111, 1085) -- the canonical chaining surface; `base_two_disk_runner` is a single composition over these methods.
- `cmd::MockRunner::with_handler` (cmd.rs:1021) -- exists, but **deliberately not used** by the mount fixture's preflight runner (a broad handler would resolve `CryptsetupIsLuks` for ProbeFailed-uncertainty tests). Reserved for per-test override at the call site if a mount test ever needs cross-cutting field-based dispatch.
- `MockRunner::with_output_stdin`'s `HashMap` insert behavior (cmd.rs:1010-1012) -- chained calls to `with_output_stdin` for the same `CmdRequest` static key overwrite, so per-test overrides on top of `base_two_disk_runner()` work without `with_handler`.

## Out of scope for this plan

- Touching `cli/src/mount.rs` production code (lines 1-827). This is a pure test-side refactor.
- Migrating other command modules (`add.rs`, `remove.rs`, `unlock.rs`, etc.) -- mount is the next migration target; siblings come in follow-up plans.
- Building a `MountTopology` / `MountPool` handler installer. The diversity of probe outcomes mount tests assert (closed/open/wrong-uuid-open/probe-failed/header-unreadable/header-damaged) makes a single broad fixture handler too narrow or too broad; flat helpers + per-test composition is the right shape, mirroring `recover.rs`'s decision.
- Promoting `NoopSleeper` to `shared.rs`. Only mount tests use it today; if a future scope needs it we move it then.
- Renaming `b"testpass"` mount fixture passphrase to match `shared::TEST_PASSPHRASE_BYTES = b"test-passphrase"`. They are intentionally different; mount tests' `with_output_stdin` expectations encode `b"testpass"` everywhere, and unifying would require touching every `with_output_stdin` call. Use `MOUNT_TEST_PASSPHRASE_BYTES = b"testpass"` in the new module.

## Risks

| # | Risk | Mitigation |
|---|---|---|
| 1 | Promoting `base_two_disk_runner` masks a future regression where a code path adds an extra probe (e.g. `CryptsetupIsLuks` against disk2) that the test author didn't intend to seed. | The promoted helper preserves the exact set of seeded requests from mount.rs:2787 verbatim. It does NOT seed `CryptsetupIsLuks`, so ProbeFailed-uncertainty tests at mount.rs:3612 (and any future ones following the pattern) continue to surface `MissingMock` -> `LuksHeaderState::ProbeFailed`. Add a one-paragraph doc comment on `base_two_disk_runner` explicitly listing what it does NOT seed and why. |
| 2 | Swapping the local `MockFs` (read_to_string returns `NotFound`) for `shared::MockFs::unmounted` (returns the rootfs-only mountinfo body) silently changes a test's behavior if any production path calls `fs.read_to_string("/proc/self/mountinfo")` from mount/probe/luks/credential_verify. | Sub-commit 1 includes a verification grep: `grep -nR "fs\.\(read_to_string\|is_block_device\|list_dir\)" cli/src/{mount,probe,luks,credential_verify}.rs`. The grep already confirms `fs.exists` is the only `Filesystem` trait method called in those production files (probe.rs:125, mount.rs:690). If a future change adds a `read_to_string` call, the test impact is contained -- the `unmounted` body says "/" rootfs is mounted (not /mnt/storage), which is the correct answer for the "pool not yet mounted" scenario most mount tests model. |
| 3 | Per-disk override of `with_output_stdin` after `base_two_disk_runner` doesn't actually overwrite (e.g. due to a future change to MockRunner switching from HashMap to a queue, OR a partial refactor that overwrites `outputs` but appends to `stdin_expectations`). | `cmd.rs:1004-1014` uses `HashMap::insert` for both `outputs` and `stdin_expectations`, which overwrites on collision. The existing `cmd::tests` block (cmd.rs:1207-1505) covers handler dispatch, stdin mismatch, header-backup side effects, and request log, but does not pin duplicate-key overwrite for either map. **Sub-commit 1 lands the required regression test** `mock_runner_with_output_stdin_override_after_base_wins` (full body in §E) that pins **both** overwrites in a single call: register two `with_output_stdin` entries for the same key with **distinct** stdin byte strings AND distinct outputs, then call `run_with_stdin` with the second entry's stdin bytes -- success implies both maps overwrote (otherwise the call panics on stdin mismatch or the resolved cmd is "first"). Mirrors the production override pattern at mount.rs:3566. Required, not optional -- the migration premise depends on it. |
| 4 | The cleanup-ordering tests' `runner.requests().iter().position(...)` and negative `!any(...)` assertions break if migration accidentally changes which requests get logged. | The migration in sub-commit 2 is import-only -- `direct_two_disk_open_runner` is moved verbatim, no change to seeded keys, no `with_handler` introduced. `MockRunner::run` always logs (cmd.rs:1172-1175) regardless of how dispatch resolves. Behavior is preserved by construction. |
| 5 | A reviewer reads the new `mount_fs(&[...])` thin wrapper and decides to "simplify" by inlining `shared::MockFs::unmounted(...)` everywhere, then later breaks the convention. | The wrapper exists for ergonomics (`&[&str]` vs `Vec<String>`) and for a single-point-of-change if mount tests ever need a different shared mock variant. Add a one-line `pub(crate) fn` doc explaining both. |
| 6 | Migration accidentally drops a `// Intent / Why it exists / Scenario` preamble during a test rewrite. | AGENTS.md's "Test Conventions" section makes the preamble part of the test contract. The verification step includes `git log -p cli/src/mount.rs` per sub-commit -- diff for each migrated test must show body changes only, with preamble lines unchanged. |

## Verification

End-to-end gate: `just test-rust` is green at every sub-commit boundary. `test-rust` (Justfile:104) takes no arguments -- it runs `cargo test --lib --test golden_nixos_25_11 --test tty_guard` as a fixed command. Filtered runs go through `cargo test` directly.

**Pre-sub-commit-1 verification (one-time):**

```
grep -nR "fs\.\(read_to_string\|is_block_device\|list_dir\)" cli/src/{mount,probe,luks,credential_verify}.rs
```

Confirms the only `Filesystem` trait method called in production by the mount-test call graph is `fs.exists()` (probe.rs:125, mount.rs:690). If this surfaces a `read_to_string` or `is_block_device` call we did not anticipate, abort the swap to `shared::MockFs::unmounted` and ship a mount-scope `MockFs` instead.

**Per sub-commit:**

- **Sub-commit 1** (scaffolding): `cargo test --manifest-path cli/Cargo.toml --lib cmd::tests::mock_runner_with_output_stdin_override_after_base_wins` passes -- pins the override contract that sub-commit 3's per-test overrides on top of `base_two_disk_runner` rely on. Then `cargo test --manifest-path cli/Cargo.toml --lib cmd::tests` confirms no regression in the existing `MockRunner` test surface. Then `cargo check --manifest-path cli/Cargo.toml --tests` clean. Then `just test-rust`.
- **Sub-commit 2** (cleanup family, import-only): `cargo test --manifest-path cli/Cargo.toml --lib mount::tests::unlock_failure_after_two_opens_closes_both_after_scoped_forget`, `cargo test --manifest-path cli/Cargo.toml --lib mount::tests::cleanup_busy_close_attempts_later_mappers_and_reports_guidance`, `cargo test --manifest-path cli/Cargo.toml --lib mount::tests::cleanup_forget_failure_warns_and_still_closes_all_mappers`, `cargo test --manifest-path cli/Cargo.toml --lib mount::tests::wrong_passphrase_zero_open_cleanup_is_noop`, then `cargo test --manifest-path cli/Cargo.toml --lib mount::tests`. Then `just test-rust`. The `forget_pos < close_pos` (mount.rs:2975) and `close_positions == vec![(forget_pos+1, ...), (forget_pos+2, ...)]` (mount.rs:3358) assertions must pass on identical request logs.
- **Sub-commit 3** (2-disk-closed collapse): for each migrated test, `cargo test --manifest-path cli/Cargo.toml --lib mount::tests::<name>`. Then `cargo test --manifest-path cli/Cargo.toml --lib mount::tests`. Then `just test-rust`. **Behavior-preservation check (manual):** `git diff` per migrated test must show body changes only -- the `// Intent / Why / Scenario` preamble lines unchanged. The set of asserts and the `open_and_mount_for_test` / `execute_*` invocation arguments must be unchanged.
- **Sub-commit 4** (3-disk and open-mappers leaf-only): same per-test verification. The "no mount mock — should never reach mount" comment at mount.rs:1350 must survive the migration verbatim, and `mount_degraded_refused` must still fail with `MountError::DegradedRefused` before reaching any `Mount` request (it would otherwise surface as `CmdError::MissingMock`, which is observable and would fail the test).
- **Sub-commit 5** (unlock-error-routing import-only): per-test, then `mount::tests`, then `just test-rust`. The ProbeFailed test at mount.rs:3612 must continue to assert `msg.contains("diagnosis could not be completed")` and `!msg.contains("single-passphrase invariant")`. If the migrated `base_two_disk_runner` accidentally seeds `CryptsetupIsLuks`, the disk2 probe would resolve to `LuksHeaderState::Ok`, the assertion would invert, and the test would fail loudly.
- **Sub-commit 6** (cleanup): `cargo check --manifest-path cli/Cargo.toml --tests` finds no dangling references; `just test-rust` full suite green. The `#[allow(dead_code)]` annotations on `test_fixtures::mount` items are removed and `cargo build` is clean (no unused warnings).

**Behavior-preservation check (mechanical, all sub-commits):**

- Every `// Intent / Why it exists / Scenario` preamble round-trips byte-for-byte.
- Every `assert!(...)` / `assert_eq!(...)` body is unchanged across the migration -- the migration touches setup code (runner, fs, params construction) only.
- Every `runner.requests().iter().any(...)` and `runner.requests().iter().position(...)` assertion observes the same request log, since the migration does not introduce `with_handler` and `MockRunner::run` always logs (cmd.rs:1172-1175).
- The `MOUNT_TEST_PASSPHRASE_BYTES = b"testpass"` constant matches every `with_output_stdin` expectation today (verified by `grep -n 'b"testpass"' cli/src/mount.rs` -- 27 occurrences in the test mod, all consistent).

No new VM tests, no parser-fixture refresh, no production behavior change. The existing test suite IS the verification.

## Branch and commit shape

Work on a feature branch (e.g. `refactor-mount-test-fixtures`). Each numbered sub-commit above is one git commit. PR opens once sub-commit 6 lands. Reviewer can walk the branch commit-by-commit; each commit is independently green.

Conventional Commits-style messages (lowercase first word per AGENTS.md):
- `refactor(test): add mount-scope test fixture module and pin MockRunner stdin override contract` (sub-commit 1)
- `refactor(mount): migrate cleanup tests to shared mount fixtures` (sub-commit 2)
- `refactor(mount): collapse 2-disk-closed preflight to base_two_disk_runner` (sub-commit 3)
- `refactor(mount): migrate 3-disk and open-mapper tests to shared leaf helpers` (sub-commit 4)
- `refactor(mount): migrate unlock-error-routing tests to shared mount fixtures` (sub-commit 5)
- `refactor(mount): drop migrated locals from mount tests module` (sub-commit 6)
