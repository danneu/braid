# Plan: pin the execute-time UUID re-probe in existing-keyfile mode

## Context

`EnrollPlan::execute` (`cli/src/enroll_key_file.rs#execute`) re-probes every
candidate's live LUKS UUID against the value discovery validated, right after the
passphrase prompt and before any `luksAddKey`. This is the decision-024
mutation-boundary guard: the passphrase prompt is an operator-controlled window in
which a disk can be swapped or reformatted to a foreign LUKS container that shares
the pool passphrase. The loop runs **unconditionally** for both enroll modes:

```rust
for c in &self.candidates {
    reprobe_member_luks_uuid(runner, &c.name, &c.by_id, &c.uuid)?;
}
```

**The gap (verified):** the only test that drives a *swapped* disk through
`execute()` and asserts rejection -- `execute_rejects_swapped_disk_before_mutation`
-- uses `generate: true`. The `reprobe_member_luks_uuid_*` unit tests call the
helper directly, bypassing `execute()`. The only `generate: false` test that
reaches the apply phase (`cmd_enroll_apply_failure_does_not_write_pending_op_journal`)
uses matching UUIDs. The VM test `tests/cli/enroll-uuid-mismatch.py` is
`--generate`-only and covers the *pre-command discovery* case, not the in-window
execute-boundary swap.

So a regression that gated the loop behind `if self.generate { ... }` would pass
**every** existing test while silently dropping the guard for the more common
operation: `braid enroll DIR` against a USB keyfile already on disk. This plan adds
one behavioral test that pins the guard's mode-independence.

This is purely additive test coverage -- no behavior change, so no principle/ADR
edits are required: the invariant is already documented in ADR 024's
[Tests That Enforce This](../../docs/design/decisions/024-luks-uuid-identity.md#tests-that-enforce-this)
section, which records enroll's execute-time re-probe and "the discovery->execute
window closure" -- exactly the property this test extends to existing-keyfile mode.

## The change

One new `#[test]` in `cli/src/enroll_key_file.rs`, placed immediately after
`execute_rejects_swapped_disk_before_mutation` (in the
`// ---- reprobe_member_luks_uuid tests ----` section). It mirrors that test with
exactly three deltas:

1. `generate: false`
2. a real existing keyfile on disk via `enroll_make_existing_keyfile` (so
   `plan_enroll`'s `validate_user_keyfile_path` passes and `execute()` is reached)
3. **no mountpoint mock** -- the mountpoint gate is `generate`-only on both the plan
   and recheck paths, so existing-keyfile mode never issues `MountpointCheck`

The swap is placed on `disk1` (the first candidate) so the re-probe loop aborts on
its first iteration; `disk2`'s second `luksUUID` call never happens.

### Reference implementation

```rust
// Intent: the discovery->execute window is closed for the EXISTING-KEYFILE
//   path too -- a disk swapped to a foreign LUKS container during the
//   passphrase prompt is rejected at the execute-time re-probe in
//   `braid enroll DIR` (no --generate) mode, before any keyfile lands in
//   slot 1.
// Why it exists: the re-probe loop in EnrollPlan::execute runs
//   unconditionally for both modes, but its sibling
//   `execute_rejects_swapped_disk_before_mutation` only drives --generate.
//   A regression that gated the loop behind `if self.generate { ... }` would
//   pass every other test while silently dropping the decision-024
//   mutation-boundary guard for the more common existing-keyfile operation
//   (braid enroll DIR against a USB keyfile already on disk). This pins the
//   guard's mode-independence. No VM test -- an in-window physical swap
//   during the passphrase prompt is not deterministically reproducible in a
//   NixOS VM; tests/cli/enroll-uuid-mismatch.py covers the pre-command
//   discovery case (--generate only).
// Scenario: 2-disk pool, existing braid.key on the USB. disk1's by-id slot
//   still matches at discovery but is swapped to a foreign LUKS volume
//   before execute re-probes it.
#[test]
fn execute_rejects_swapped_disk_existing_keyfile_before_mutation() {
    let (tmp, paths) = isolated_paths();
    let (kf, _kf_str) = enroll_make_existing_keyfile(&tmp);
    let pass_path = tmp.path().join("pass");
    std::fs::write(&pass_path, "testpass\n").unwrap();

    let d1 = "/dev/disk/by-id/d1";
    let d2 = "/dev/disk/by-id/d2";
    let foreign = "ffffffff-ffff-ffff-ffff-ffffffffffff";

    // Mappers closed => discovery issues exactly one luksUUID per disk, so the
    // 2nd sequence element is consumed by the execute re-probe. A mapper-open
    // disk would pop both at discovery and surface the mismatch at plan time
    // instead of the execute boundary under test.
    let (_, d1_match) = enroll_luks_uuid_ok(d1, test_uuid(500).as_str());
    let (_, d1_swapped) = enroll_luks_uuid_ok(d1, foreign);
    let (d2_req, d2_out) = enroll_luks_uuid_ok(d2, test_uuid(501).as_str());
    let runner = MockRunner::default()
        .with_output_sequence(
            CmdRequest::CryptsetupLuksUuid { device: d1.to_owned() },
            vec![d1_match, d1_swapped],
        )
        .with_output(d2_req, d2_out)
        .with_luks_dump_text_luks2_for(&[d1, d2])
        .with_mappers_closed(&["braid-disk1", "braid-disk2"]);
    // No mountpoint mock: existing-keyfile mode skips the generate-only
    // mountpoint gate on both the plan and recheck paths.

    let fs = enroll_fs(&[d1, d2]);
    let membership = enroll_make_membership(&[("disk1", d1), ("disk2", d2)]);

    let params = EnrollKeyFileParams {
        membership: &membership,
        key_file_path: &kf,
        generate: false,
        passphrase_stdin: false,
        passphrase_file: Some(&pass_path),
        dry_run: false,
        paths: &paths,
        backing_path_resolver: crate::test_fixtures::mock_virtio_backing_path_resolver(),
    };

    let plan = plan_enroll(&runner, &fs, &params)
        .expect("discovery must succeed with matching UUIDs");
    let err = plan
        .execute(&runner, &params)
        .expect_err("execute re-probe must reject the swapped disk")
        .to_string();

    assert!(err.contains("LUKS UUID mismatch"), "expected mismatch error: {err}");
    assert!(err.contains("disk1"), "error should name the swapped disk: {err}");
    assert!(
        !runner
            .requests()
            .iter()
            .any(|r| matches!(r, CmdRequest::CryptsetupLuksAddKeyFile { .. })),
        "no keyfile may be enrolled after a swap is detected: {:?}",
        runner.requests()
    );
    assert!(kf.exists(), "the existing user keyfile must be left untouched");
}
```

All referenced fixtures already exist and are imported in the test module:
`enroll_make_existing_keyfile`, `enroll_luks_uuid_ok`, `enroll_make_membership`,
`enroll_fs`, `isolated_paths`, `test_uuid` (all `crate::test_fixtures`),
`MockRunner::with_output_sequence` / `with_luks_dump_text_luks2_for` /
`with_mappers_closed` (`cli/src/cmd.rs`). No new fixtures or production code.

## Why this shape (alternatives rejected)

- **Not a structural change.** The loop is already mode-independent; there is
  nothing to restructure to make the regression "impossible" without adding
  complexity. The correct defense for a mode-independence property is a behavioral
  test that pins it.
- **Not a `generate`-parameterized refactor of the two execute-swap tests.** This
  codebase's idiom is one named test per scenario with an Intent/Why/Scenario
  preamble; it uses no `rstest`-style parameterization. The ~40 lines of overlap
  with the sibling are idiomatic, and the two tests document distinct scenarios.
- The new test is leaner than its `generate: true` sibling: because the re-probe
  aborts before `plan_enrollment`, it needs no keyfile-probe, passphrase-verify, or
  slot-1 (`luksDump`) mocks, and no mountpoint mock.

## Files modified

- `cli/src/enroll_key_file.rs` -- add the one test above. No other files.

## Verification

1. **Test passes as written:**
   `cargo test -p braid --lib execute_rejects_swapped_disk_existing_keyfile_before_mutation`
   (or `just test-rust`). Confirm green.
2. **Prove it guards the gap (TDD discipline -- confirm it fails for the right
   reason).** Temporarily edit `EnrollPlan::execute` to gate the loop:
   ```rust
   if self.generate {
       for c in &self.candidates {
           reprobe_member_luks_uuid(runner, &c.name, &c.by_id, &c.uuid)?;
       }
   }
   ```
   Re-run the full enroll test module. Expect: the **new** test FAILS on the
   `assert!(err.contains("LUKS UUID mismatch"))` assertion (line 124 above). With the
   re-probe skipped in existing-keyfile mode, execution progresses past the
   now-skipped re-probe into `plan_enrollment`'s credential verification, which issues
   an unmocked passphrase-verify command; `MockRunner` returns a missing-mock
   `CmdError` (it errors, it does not panic -- see
   `cli/src/cmd.rs#mock_runner_requests_records_missing_mock_calls_too`), so
   `execute()` returns a non-mismatch error and the mismatch-wording assertion fails
   first. `execute_rejects_swapped_disk_before_mutation` still PASSES (it is
   `generate: true`, so its re-probe still runs). Revert the gate.
3. **Full suite:** `just test-rust` to confirm no collateral breakage (the manual
   runner build and sequence wiring match the existing sibling, so no fixture drift).
