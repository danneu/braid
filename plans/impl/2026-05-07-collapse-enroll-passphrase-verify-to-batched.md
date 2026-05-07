# Collapse enroll passphrase verify to a single batched call

## Context

`plan_enrollment` in `cli/src/enroll_key_file.rs:150-266` calls
`verify_credential_for_targets` from two sites:

1. `:158-180` -- pre-loop, on the first candidate only.
2. `:222-254` -- inside the per-candidate loop, guarded by `i > 0`.

Each site duplicates the same `match`-on-`CredentialVerifyError::{Rejected,
Luks}` arms and emits a slightly different error string for the same
condition: `"wrong passphrase (verified against {})"` (pre-loop) vs
`"wrong passphrase on {}"` (per-disk). The split exists because:

- The first verify must fire BEFORE any keyfile-probe stderr leaks
  (`tests/cli/braid-enroll.py:235-278`, test 4b).
- Per-disk verifies cover divergent passphrases on later candidates
  (`tests/cli/braid-enroll.py` regression added in commit `7ca2acd`,
  pinned by the `plan_divergent_passphrase_existing_keyfile_errors_on_disk2`
  unit test).

Sibling commands (`mount.rs:493`, `add.rs:690`, `replace.rs:370`) all
build a `Vec<CredentialVerifyTarget>` over their full target set and
call `verify_credential_for_targets` exactly once. `enroll` is the
outlier. The intent is to collapse to one call site, one error string,
and match sibling shape -- without regressing test 4b.

A naive flip ("probe first, then verify only the `NeedsEnroll`
subset") was considered and rejected: it would emit `[ok] keyfile:
already enrolled on diskN` lines BEFORE any passphrase verify, which
test 4b explicitly forbids; and in the all-`AlreadyEnrolled` case the
slice is empty and `verify_credential_for_targets(&[])` is a no-op (per
`verify_credential_for_targets_empty_list_is_ok` in
`cli/src/credential_verify.rs:307-324`), so a wrong passphrase would
silently succeed.

The right pivot keeps the verify FIRST -- but applies it to ALL
candidates in one batched call.

## Goal

In `plan_enrollment`:

- Single `verify_credential_for_targets` call site, over all candidates,
  before the keyfile-probe loop.
- Single error string: `"wrong passphrase on {target.name}"`.
- Drop the `i > 0` guard on the loop iteration counter.
- Remove the stale `verify_first_candidate_passphrase` reference at
  `:227` (the function was inlined in commit `1825997`).

## Approach

Edit `cli/src/enroll_key_file.rs::plan_enrollment` (`:150-266`).

1. Replace the pre-loop block (`:157-180`) with a batched verify over
   all candidates:

   ```rust
   let color_enabled = color_enabled_for_stderr();
   let verify_targets: Vec<CredentialVerifyTarget> = candidates
       .iter()
       .map(|(name, by_id)| CredentialVerifyTarget {
           name: name.clone(),
           device: by_id.0.clone(),
       })
       .collect();
   match verify_credential_for_targets(
       runner,
       &verify_targets,
       Credential::Passphrase(passphrase),
       color_enabled,
       |line| eprint!("{line}"),
   ) {
       Ok(()) => {}
       Err(CredentialVerifyError::Rejected { target }) => {
           return Err(EnrollKeyFileError::Validation(format!(
               "wrong passphrase on {}",
               target.name
           )));
       }
       Err(CredentialVerifyError::Luks { source, .. }) => {
           return Err(EnrollKeyFileError::Luks(source));
       }
   }
   ```

2. In the candidate loop, drop the `(i, ...)` enumeration -- just
   `for (name, by_id) in candidates`. Delete the per-iteration verify
   block (`:222-254`) entirely.

3. Update the loop's leading comment: it currently explains why the
   first-candidate verify is unconditional and why per-iteration verify
   skips `i == 0`. Replace with a short note that all candidates were
   verified up-front (matches sibling commands).

## Stderr ordering after the change

Real-run, both disks need enroll:

```
[wait] passphrase: checking against disk1...
[ok]   passphrase: accepted by disk1
[wait] passphrase: checking against disk2...
[ok]   passphrase: accepted by disk2
[wait] keyfile: checking against disk1...
[skip] keyfile: not yet enrolled on disk1
enroll: disk1 -- will add keyfile to slot 1
[wait] keyfile: checking against disk2...
[skip] keyfile: not yet enrolled on disk2
enroll: disk2 -- will add keyfile to slot 1
```

Idempotent re-enroll, both already enrolled:

```
[wait] passphrase: checking against disk1...
[ok]   passphrase: accepted by disk1
[wait] passphrase: checking against disk2...
[ok]   passphrase: accepted by disk2
[wait] keyfile: checking against disk1...
[ok]   keyfile: already enrolled on disk1
[wait] keyfile: checking against disk2...
[ok]   keyfile: already enrolled on disk2
```

Wrong passphrase (test 4b -- both already enrolled):

```
[wait] passphrase: checking against disk1...
<error: "wrong passphrase on disk1">
```

Divergent passphrase on disk2:

```
[wait] passphrase: checking against disk1...
[ok]   passphrase: accepted by disk1
[wait] passphrase: checking against disk2...
<error: "wrong passphrase on disk2">
```

## Tradeoff

Idempotent re-enroll on an N-disk pool now does N passphrase verifies
(today: 1). Each `cryptsetup --test-passphrase` is one PBKDF derivation
-- ~1 s with default Argon2 settings, <1 ms with the pbkdf2-1000 test
settings. For the typical 2-4 disk NAS, ~1-3 extra seconds on a rare
idempotent re-enroll. Net win for code clarity and consistency with
sibling commands.

Side effect: divergent-passphrase rejection now happens before any
keyfile probe state is emitted -- cleaner gate, faster surface. No
test pins the prior interleaving.

## Critical files

- `cli/src/enroll_key_file.rs` -- implementation edit (body of
  `plan_enrollment`, `:150-266`) plus the three test updates and one
  preamble reword called out in the "Test impact" section below.
- `manual/commands/enroll.md` -- docs edit called out in the "Docs
  impact" section below.

## Reuse

- `verify_credential_for_targets` (`cli/src/credential_verify.rs:30-73`)
  -- already used by mount/add/replace.
- `CredentialVerifyTarget`, `CredentialVerifyError` -- already imported
  at `cli/src/enroll_key_file.rs:3-5`.

## Test impact

Tests that need updates:

- **`plan_all_already_enrolled`** -- TODAY only seeds a `CryptsetupTestPassphrase`
  mock for d1 because the per-iter `i > 0` verify is skipped via the
  `AlreadyEnrolled` `continue`. After the pivot, the batched verify hits
  d2 too, so the test will hit `MissingMock`. Seed the d2 passphrase mock
  AND tighten the test to assert request order on `runner.requests()`:
  `[Passphrase{d1}, Passphrase{d2}, KeyFile{d1}, KeyFile{d2}]`. Pins the
  new "verify all candidates first, then probe" contract for the
  idempotent path.

- **`plan_divergent_passphrase_existing_keyfile_errors_on_disk2`** --
  TODAY only asserts the error contains `"wrong passphrase"` and `"disk2"`,
  which the OLD probe-then-verify ordering also satisfies. Tighten to
  assert the request log is exactly `[Passphrase{d1}, Passphrase{d2}]` --
  no `CryptsetupTestKeyFile`, no `CryptsetupLuksDump`. Drop the now-
  unreachable `tkf1`/`tkf2`/`ld1` mocks from the setup. Reword the
  "Why it exists" preamble: the regression mode is now "skipping a
  candidate from the batched-verify slice", not "skipping the per-disk
  passphrase verify".

- **`plan_wrong_passphrase_errors`** -- TODAY substring-matches `"wrong
  passphrase"`. Tighten to assert the exact error string `"wrong
  passphrase on disk1"`. Pins the new canonical wording so a regression
  that re-introduces `"wrong passphrase (verified against ...)"` cannot
  survive.

Test that needs a comment-only update (assertions still pass):

- **`plan_generate_new_does_not_repeat_first_candidate_passphrase_verify`**
  -- the test still passes (each disk's `CryptsetupTestPassphrase` is
  invoked exactly once under the batched verify), but its preamble
  describes the removed structure ("verifies disk1 before the loop and
  verifies later disks inside the loop ... regression that drops the
  `i > 0` guard would verify disk1 twice"). Reword the preamble to
  describe the new contract: the batched up-front verify must include
  each candidate exactly once.

Tests that should pass without modification:

- `plan_all_need_enroll`, `plan_mixed_enrolled_and_needs`,
  `plan_slot1_conflict_errors`,
  `plan_keyfile_verify_busy_surfaces_open_failed_not_proceeds`
  (all in `cli/src/enroll_key_file.rs::tests`). Each already seeds
  passphrase mocks for both disks (or has a single candidate), and
  none lock the prior probe-then-verify ordering.

- VM tests `tests/cli/braid-enroll.py`, `braid-enroll-generate.py`,
  `braid-add-enroll.py`. Two assertion shapes appear:
    - **Exact-equality dry-run stderr** (Tests 1b and 4d Phase A use
      `assert err == expected_err` over the full stderr buffer). These
      run through `plan_enroll`'s dry-run probe path, not
      `plan_enrollment`, so they are unaffected by this change.
    - **Substring + pairwise-ordering** (Tests 1, 1a, 3, 4, 4b, 4c,
      4d Phase B, 5 plus everything in the other two files) for
      real-run paths. These are satisfied by the new "all passphrase
      verifies, then all keyfile probes/enroll lines" stream.

`MockRunner` tolerates unused mocks (`cli/src/cmd.rs:955-1006`: it
only fails on `MissingMock`), so seeded-but-unused mocks do not break
tests -- but pruning them on the divergent-passphrase test is still
worth it for clarity.

## Docs impact

`manual/commands/enroll.md:63` currently reads:
> Verifies the passphrase against the first pool disk.

After the pivot, the passphrase is verified against every present LUKS
candidate before any keyfile probe runs. Update that bullet to read
something like "Verifies the passphrase against every present pool
disk before any keyfile probe." The "Safety checks" line at `:76`
("Passphrase is verified before any mutations.") is still accurate
and can be left or tightened.

## Verification

1. `just test-rust` -- unit tests pass.
2. `just test-vm braid-enroll braid-enroll-generate braid-add-enroll`
   -- the three VM tests that exercise enroll surfaces pass.
3. Eyeball the resulting `plan_enrollment` body to confirm the loop
   has no `i`, no second `verify_credential_for_targets` call, and no
   stale `verify_first_candidate_passphrase` reference.
