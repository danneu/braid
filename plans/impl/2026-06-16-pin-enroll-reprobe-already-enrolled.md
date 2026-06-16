# Pin execute-time UUID re-probe coverage for AlreadyEnrolled candidates

## Context

`braid enroll`'s `EnrollPlan::execute` re-probes every present member's live
LUKS UUID at the mutation boundary, before any keyfile lands in slot 1
(decision-024: re-check member identity at every mutation boundary, because the
passphrase prompt is an operator-controlled window in which a disk could be
swapped to a foreign LUKS container that shares the pool passphrase). The loop
iterates `self.candidates` and runs *before* `plan_enrollment` classifies each
candidate as `AlreadyEnrolled` / `NeedsEnroll`:

- `cli/src/enroll_key_file.rs:551-553` -- the candidate-driven re-probe loop.
- `cli/src/enroll_key_file.rs:230-252` -- `reprobe_member_luks_uuid` (reject +
  fail-closed arms).
- `cli/src/enroll_key_file.rs:560` -- `plan_enrollment` (classification) runs
  *after* the loop.
- `cli/src/enroll_key_file.rs:285-311` -- `GenerateNew` always yields
  `NeedsEnroll`; only `ExistingKeyfile` mode can yield `AlreadyEnrolled`.

**The gap.** Both existing swap tests drive `NeedsEnroll` candidates:
`execute_rejects_swapped_disk_before_mutation` (`:3621`, generate mode -> always
`NeedsEnroll`) and `execute_rejects_swapped_disk_existing_keyfile_before_mutation`
(`:3714`, rejected at the re-probe before `plan_enrollment` ever classifies, and
mocks no keyfile probe). No test exercises a candidate that `plan_enrollment`
*would* classify `AlreadyEnrolled`. Two plausible refactors would silently drop
the swap check on an already-enrolled disk:

- **(A)** Relocate the loop to run *after* `plan_enrollment` (over all actions):
  the swap is still caught, but only after the keyfile probe has issued a
  credential test against the foreign container.
- **(B)** Filter the loop to `NeedsEnroll` actions only ("why re-probe disks we
  won't mutate?"): the swap on an `AlreadyEnrolled` disk goes entirely
  undetected and `execute` reports success. In a multi-member pool, a legit
  `NeedsEnroll` sibling would then get enrolled while a foreign disk sits in the
  pool unnoticed.

Honest framing of severity (Low): both existing swap tests *do* fail under
(A)/(B) today, but only indirectly -- they seed no passphrase/keyfile-probe
mocks, so once the re-probe moves after `plan_enrollment`,
`verify_credential_for_targets`
(`cli/src/credential_verify.rs#verify_credential_for_targets`) hits
`MissingMock` before any UUID mismatch can surface, and the `"LUKS UUID
mismatch"` assertion fails with a misleading error. Nothing exercises the real
`AlreadyEnrolled` classification, and nothing pins the candidate-vs-action
*ordering* (swap caught before the keyfile probe). This plan closes that
precisely.

**Outcome.** One new regression-guard unit test. No production code changes --
the re-probe is already candidate-driven and correct.

## Change

Add a single `#[test]` to the `tests` module of `cli/src/enroll_key_file.rs`,
immediately after `execute_rejects_swapped_disk_existing_keyfile_before_mutation`
(ends at `:3785`), before the `// ---- generate_key_file tests ----` divider
(`:3787`). It is a close 1-disk sibling of that test, plus (1) passphrase +
keyfile-probe "regression-bait" mocks that put the candidate on the
`AlreadyEnrolled` path, and (2) an explicit assertion that no keyfile probe runs
before the swap is rejected.

```rust
// Intent: the execute-time UUID re-probe covers EVERY present member,
//   including a candidate that plan_enrollment would classify
//   AlreadyEnrolled. A disk swapped to a foreign LUKS container that
//   shares the pool passphrase and already holds the keyfile in slot 1 is
//   still rejected at the mutation boundary -- before any mutation, and
//   before the keyfile probe even runs.
// Why it exists: the re-probe loop in EnrollPlan::execute iterates
//   `self.candidates` and runs BEFORE plan_enrollment classifies them.
//   Two refactors would drop the swap check on an already-enrolled disk:
//     (A) relocating the loop to run AFTER plan_enrollment (over all
//         actions) -- the keyfile probe would then test a credential
//         against the foreign container before the swap is caught;
//     (B) filtering the loop to NeedsEnroll actions only -- the swap on an
//         AlreadyEnrolled disk would go entirely undetected and execute
//         would report success.
//   The sibling swap tests both drive NeedsEnroll candidates and seed no
//   passphrase/keyfile-probe mocks, so under (A)/(B) they fail only
//   indirectly: plan_enrollment's verify_credential_for_targets hits
//   MissingMock before any UUID mismatch can surface, and neither test
//   exercises the AlreadyEnrolled classification nor pins that the
//   re-probe runs before the keyfile probe. This test seeds those mocks
//   (UNCONSUMED in correct code, since the re-probe rejects first) so that
//   under (A)/(B) plan_enrollment fully classifies the disk AlreadyEnrolled
//   and the regression surfaces as a clean failed assertion here, not a
//   MissingMock error. No VM test -- an in-window physical swap during the
//   passphrase prompt is not deterministically reproducible in a NixOS VM;
//   tests/cli/enroll-uuid-mismatch.py covers the pre-command discovery case.
// Scenario: 1-disk pool, existing braid.key on the USB, idempotent re-run.
//   disk1's by-id slot matches at discovery but is swapped to a foreign
//   LUKS volume (whose slot 1 already holds the keyfile and shares the
//   passphrase) before execute re-probes it.
#[test]
fn execute_rejects_swapped_already_enrolled_disk_before_mutation() {
    let (tmp, paths) = isolated_paths();
    let (kf, kf_str) = enroll_make_existing_keyfile(&tmp);
    let pass = "testpass";
    let pass_path = tmp.path().join("pass");
    std::fs::write(&pass_path, format!("{pass}\n")).unwrap();

    let d1 = "/dev/disk/by-id/d1";
    let foreign = "ffffffff-ffff-ffff-ffff-ffffffffffff";

    // Mapper closed => discovery issues exactly one luksUUID for d1, so the
    // 2nd sequence element is consumed by the execute-time re-probe.
    let (_, d1_match) = enroll_luks_uuid_ok(d1, test_uuid(500).as_str());
    let (_, d1_swapped) = enroll_luks_uuid_ok(d1, foreign);

    // Regression bait: if the re-probe were moved after plan_enrollment (A)
    // or filtered to NeedsEnroll (B), plan_enrollment would verify the
    // passphrase and authenticate the keyfile probe -> AlreadyEnrolled.
    // In correct code these are never consumed (re-probe rejects first).
    let (tp_req, tp_stdin, tp_out) = enroll_test_passphrase_ok(d1, pass);
    let (tkf_req, tkf_out) = enroll_test_keyfile_ok(d1, &kf_str);

    let runner = MockRunner::default()
        .with_output_sequence(
            CmdRequest::CryptsetupLuksUuid {
                device: d1.to_owned(),
            },
            vec![d1_match, d1_swapped],
        )
        .with_luks_dump_text_luks2(d1)
        .with_mappers_closed(&["braid-disk1"])
        .with_output_stdin(tp_req, tp_stdin, tp_out)
        .with_output(tkf_req, tkf_out);
    // No mountpoint mock: existing-keyfile mode skips the generate-only gate.

    let fs = enroll_fs(&[d1]);
    let membership = enroll_make_membership(&[("disk1", d1)]);

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

    let plan =
        plan_enroll(&runner, &fs, &params).expect("discovery must succeed with the matching UUID");
    let err = plan
        .execute(&runner, &params)
        .expect_err("execute re-probe must reject the swap even on an already-enrolled disk")
        .to_string();

    assert!(
        err.contains("LUKS UUID mismatch"),
        "expected mismatch error: {err}"
    );
    assert!(
        err.contains("disk1"),
        "error should name the swapped disk: {err}"
    );

    let reqs = runner.requests();
    assert!(
        !reqs
            .iter()
            .any(|r| matches!(r, CmdRequest::CryptsetupLuksAddKeyFile { .. })),
        "no keyfile may be enrolled after a swap is detected: {reqs:?}"
    );
    // Candidate-driven, not action-driven: the swap is caught before the
    // keyfile probe runs. A re-probe relocated after plan_enrollment (A)
    // would issue this probe before failing.
    assert!(
        !reqs
            .iter()
            .any(|r| matches!(r, CmdRequest::CryptsetupTestKeyFile { .. })),
        "the swap must be caught before any keyfile probe runs: {reqs:?}"
    );
    assert!(
        kf.exists(),
        "the existing user keyfile must be left untouched"
    );
}
```

## Why this shape

- **1 disk, existing-keyfile, AlreadyEnrolled bait.** Matches the finding,
  minimal, and sufficient: it is the missing cell in the swap-test matrix
  (mode x classification). A 2-disk variant (one `AlreadyEnrolled`+swapped, one
  `NeedsEnroll`) tells a richer story but needs the legit disk's full
  enroll-path mocks to reach the regressed "success", with no extra
  regression-catching power.
- **Dual assertions catch both regressions.** `expect_err` + `"LUKS UUID
  mismatch"` catches (B) (regressed code returns `Ok`). The "no
  `CryptsetupTestKeyFile`" assertion catches (A) (regressed code probes the
  keyfile before failing). The no-`luksAddKey` and `kf.exists()` assertions pin
  fail-closed-before-mutation, matching the sibling tests' style.
- **Unconsumed-mock trick.** In correct code the re-probe rejects before
  `plan_enrollment`, so `CryptsetupTestPassphrase` / `CryptsetupTestKeyFile` are
  never issued. `MockRunner` returns `MissingMock` only for an *unmatched
  request* (`cli/src/cmd.rs:1541-1562`); unused mocks are fine. Those mocks make
  the regressed path classify `AlreadyEnrolled` cleanly instead of dying on a
  missing mock.
- **UUID alignment.** `enroll_make_membership(&[("disk1", d1)])` seeds disk1's
  membership UUID as `test_uuid(500)`; discovery's `d1_match` is `test_uuid(500)`
  (so discovery passes), and the carried candidate uuid is compared against the
  `foreign` value at execute (so the re-probe mismatches).

## Critical files

- `cli/src/enroll_key_file.rs` -- the only file changed (one test added to the
  `tests` module).
- Read-only reference for the fixtures used: `cli/src/test_fixtures/enroll_key_file.rs`
  (`enroll_luks_uuid_ok`, `enroll_test_passphrase_ok` [3-tuple, `with_output_stdin`],
  `enroll_test_keyfile_ok` [2-tuple, `with_output`], `enroll_make_existing_keyfile`,
  `enroll_make_membership`, `enroll_fs`) and `cli/src/test_fixtures/shared.rs`
  (`test_uuid`, `mock_virtio_backing_path_resolver`).

## Verification

1. **Run the new test** (and its siblings):
   - Full lane: `just test-rust`.
   - Targeted: `cargo test --manifest-path cli/Cargo.toml --lib execute_rejects_swapped`
     (runs all three swap tests). Expect green.
2. **Prove it is a real guard (mutation test).** Temporarily apply each
   regression in `EnrollPlan::execute`, run the proof against the new test
   specifically
   (`cargo test --manifest-path cli/Cargo.toml --lib execute_rejects_swapped_already_enrolled_disk_before_mutation`),
   confirm it fails for the right reason, then revert:
   - **(B)** Move the `for c in &self.candidates { reprobe_member_luks_uuid(...) }`
     loop to *after* `plan_enrollment` and iterate only the returned
     `NeedsEnroll` actions. The new test must fail at `expect_err` -- execute
     now returns `Ok` (the AlreadyEnrolled disk is never re-probed).
   - **(A)** Move the loop to after `plan_enrollment` but keep it over all
     candidates. The new test must fail at the no-`CryptsetupTestKeyFile`
     assertion (the keyfile probe ran first); the `"LUKS UUID mismatch"` error
     itself still holds.

   Note: the two existing swap tests also fail under both mutants, but only
   indirectly -- with no passphrase/keyfile-probe mocks, `plan_enrollment`
   hits `MissingMock` before the mismatch surfaces, so their failure is a
   misleading `MissingMock` rather than a clean behavioral signal. That
   confusing failure mode is exactly the gap the new test closes, so read the
   proof off the new test, not the siblings.
3. **Lint/format:** `just clippy` (Rust lints) and
   `cargo fmt --manifest-path cli/Cargo.toml --check` (Rust formatting; the
   justfile has no bare `fmt` recipe -- `fmt-nix` covers only Nix sources).
