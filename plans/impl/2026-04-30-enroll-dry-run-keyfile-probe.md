# Plan: dry-run faithfulness for `braid enroll`

## Context

`braid enroll --dry-run` lists every LUKS candidate as a slot-1 enroll step,
even disks whose keyfile is already installed. The real run silently skips
those via `plan_enrollment`'s `AlreadyEnrolled` branch
(`cli/src/enroll_key_file.rs`, in `plan_enrollment`), so the preview
overstates the work.

This is cosmetic, not unsafe -- slot-1 preflight (`check_slot_one_available`)
and the actual probe still run at execute time. But it contradicts
decision-012's "intent CLI" promise that dry-run is a faithful preview, and
it is the only intent-CLI dry-run path that overstates work despite having
the information needed to be accurate.

The fix is achievable without expanding the credential model: the probe
(`luks::verify_key_file`, in `cli/src/luks.rs`) takes only the keyfile
path -- no passphrase. So dry-run can probe per-disk for free.

## Approach

Thread an explicit `dry_run: bool` into `plan_enroll` and gate the new
probe on `dry_run && !generate`. The probe must NOT run on real executions.

Current real-run flow inside `cmd_enroll_key_file` -> `plan_enroll` ->
`EnrollPlan::execute` -> `plan_enrollment`:

1. Discovery (`discover_enrollment_candidates`)
2. `read_passphrase`
3. First-disk passphrase verify via `verify_credential_for_targets`
   with `Credential::Passphrase(passphrase)` against the first
   candidate (replaces the older `verify_first_candidate_passphrase`).
4. For each `(name, by_id)`:
   a. `luks::verify_key_file` (per-disk keyfile probe) -- on
      `Authenticated`, push `AlreadyEnrolled` and `continue`.
   b. If `i > 0`: per-disk passphrase verify via the same
      `verify_credential_for_targets` /
      `Credential::Passphrase(passphrase)` helper (replaces the older
      direct `luks::verify_passphrase` call) -- added to surface
      divergent-passphrase pools before partial mutation.
   c. `check_slot_one_available` (slot-1 preflight).
   d. Push `NeedsEnroll`.
5. `apply_enrollment` for `NeedsEnroll` items only.

The dry-run change must touch only step 1 + a new gated probe; it must
leave 2/3/4a/4b/4c/4d/5 untouched. In particular, the
`verify_credential_for_targets` calls at steps 3 and 4b and the
`check_slot_one_available` preflight at 4c remain the sole property of
the execute path. Hoisting the keyfile probe into the non-dry-run
`plan_enroll` would re-order operations (probe errors before the
passphrase prompt, double-emission of `ok:` lines), which is exactly
the bug the gate prevents.

When the probe runs (dry-run, not generate, after discovery succeeds):

1. Per `(name, by_id)` candidate, call
   `luks::verify_key_file(runner, &by_id.0, key_file_path)`.
2. On `Authenticated`: drop that candidate from the list passed to
   `compile_enroll_steps` (no enroll-step + no header-backup-step) and
   append a `PreviewNote::PerDisk { name, level: NoteLevel::Skip,
   message: "keyfile already enrolled" }` to the `notes` vector that
   feeds `EnrollPlan::preview()`.
3. On `Rejected`: keep the candidate for step compilation, no note.
4. On any other probe error: return early via `EnrollPlanReport`,
   preserving any skip notes accumulated so far -- mirroring how
   `discover_enrollment_candidates` propagates mid-loop errors.

Because the probe is gated on `dry_run`, real runs build an `EnrollPlan`
whose `notes` contain only discovery notes (today's behavior). Real-run
stderr stays byte-for-byte identical: `plan_enrollment` keeps emitting
`ok: <name> -- keyfile already enrolled`; no double-print risk because
the dry-run plan and the real-run plan are built by separate
`plan_enroll` invocations from `cmd_enroll_key_file` -- one `EnrollPlan`
is never used for both paths.

`EnrollPlan::candidates` stays unfiltered so `execute()` ->
`plan_enrollment` still sees the full pool and re-probes for safety. The
dry-run probe is a preview-fidelity boost only; it is never
authoritative for mutations.

## Critical files

- **cli/src/enroll_key_file.rs**
  - `plan_enroll`: take `dry_run: bool`, run the probe loop when
    `dry_run && !generate`. (Symbol search: `pub fn plan_enroll`.)
  - `compile_enroll_steps`: unchanged signature; called with the
    filtered `NeedsEnroll`-only candidate slice when probing happened.
  - `EnrollPlan` and `EnrollPlan::execute`: unchanged. Notes vector
    serves both dry-run and real-run, but real-run plans never receive
    probe-derived skip notes (gate is in `plan_enroll`).
  - `cmd_enroll_key_file`: pass `params.dry_run` to `plan_enroll`.
  - `plan_enrollment`: explicitly NOT modified -- the first-disk and
    per-disk passphrase verifies (both via `verify_credential_for_targets`
    with `Credential::Passphrase(...)`) and the
    `check_slot_one_available` preflight stay where they are.
- **docs/decisions/012-intent-cli.md**
  - The `--dry-run` paragraph (currently around line 51) reads:
    `"--dry-run reads the LUKS label without side effects. Full
    identity verification (FSID comparison) requires opening the
    mapper, so dry-run defers this to execution time when the mapper
    is closed."` This phrasing is narrower than the new behavior --
    the keyfile probe is also a read-only, side-effect-free check,
    but it is more than just reading the LUKS label. Amend the
    paragraph to affirmatively allow side-effect-free,
    passphrase-free LUKS probes (label reads and the keyfile test
    via `cryptsetup open --test-passphrase --key-file`, which
    evaluates a credential without activating the device -- the
    invocation matches `CryptsetupTestKeyFile` in `cli/src/cmd.rs`),
    and keep deferring checks that require a passphrase or an open
    mapper (FSID comparison) to execution time. Keep the change to a
    single paragraph; do not restructure surrounding sections.
- **tests/cli/braid-enroll.py**
  - `Test 1b`: currently runs `braid enroll /tmp --dry-run` after Test 1
    has already enrolled both disks, then asserts stderr is empty and
    `"enroll keyfile" in out`. The block-comment header explicitly
    documents the bug as intended ("planner classification is
    post-passphrase and bypassed in dry-run"). After the fix, both
    disks are already enrolled, so the dry-run probe should classify
    both as `Authenticated`. Update assertions to: `"enroll keyfile"`
    is **absent** from stdout; the bracketed-style PerDisk lines
    `[skip] disk disk1: keyfile already enrolled` and
    `[skip] disk disk2: keyfile already enrolled` are present (this
    is the literal wording produced by `Preview`'s `Bracketed`
    PerDiskStyle, `format!("disk {name}: {message}")` -- see
    `format_per_disk_line` in `cli/src/preview.rs`); and the preview
    ends with `Preview::print_colored`'s nothing-to-do footer (verify
    the exact wording against `cli/src/preview.rs` while implementing
    -- do not invent it). Preserve `err == ""`. Rewrite the
    block-comment header so the lock now reads "dry-run probes
    enrollment state pre-passphrase via the passphrase-free
    `verify_key_file` call."
  - `Test 3`: unaffected -- real-run stderr still emits
    `ok: <name> -- keyfile already enrolled` from `plan_enrollment`.

## Reuse

- `luks::verify_key_file` -- the probe; same call used by
  `plan_enrollment`. Returns `Ok(VerifyOutcome::Authenticated)` /
  `Ok(VerifyOutcome::Rejected)` / `Err(...)` for busy/missing/generic.
  No passphrase argument.
- `PreviewNote::PerDisk` + `NoteLevel::Skip` (`cli/src/preview.rs`) --
  existing per-disk Skip note variant; matches the discovery path's
  "skip: <name> not present" wording family.
- `EnrollKeyFileError` -- propagate any non-Rejected probe exit
  (busy / not-found / generic) the same way `plan_enrollment`'s `?`
  propagates them at execute time.

## Behavior matrix

| Scenario                          | Today's dry-run                    | After fix                                  |
| --------------------------------- | ---------------------------------- | ------------------------------------------ |
| `--generate`, no keyfile present  | enroll + backup steps per disk     | unchanged                                  |
| Existing keyfile, fully unenrolled | enroll + backup steps per disk     | unchanged                                  |
| Existing keyfile, fully enrolled  | enroll + backup steps per disk     | zero steps + per-disk Skip notes + `nothing to do.` footer |
| Existing keyfile, mixed pool      | enroll + backup steps per disk     | steps for unenrolled disks only + Skip notes for the rest |
| Probe error (busy/missing) in dry-run | enroll + backup steps (real run errors later) | dry-run errors, mirroring real-run probe failure |
| Real run, any keyfile state       | unchanged                          | unchanged (probe stays gated to dry-run)   |

## Verification

End-to-end:

1. **Unit tests** (in `cli/src/enroll_key_file.rs#[cfg(test)] mod tests`):
   - `dry_run_skips_already_enrolled_disks`: two candidates, mock
     `CryptsetupTestKeyFile` to return exit 0 (Authenticated) for one
     and exit 2 (Rejected) for the other. Assert the rendered preview
     contains an enroll+backup pair only for the Rejected device, and a
     `PerDisk { level: Skip }` line for the Authenticated one. Use
     `assert_eq!` on the full rendered preview buffer to pin exact
     wording (per memory: pin preservation claims with exact-equality
     assertions on the full buffer, not substrings).
   - `dry_run_all_already_enrolled_emits_zero_steps`: every candidate
     returns Authenticated. Assert `Preview.steps.is_empty()` and one
     Skip note per candidate; assert the rendered footer line.
   - `dry_run_with_generate_skips_probe`: `--generate` mode. Assert
     `runner.requests()` contains zero
     `CmdRequest::CryptsetupTestKeyFile` entries -- pinning that the
     keyfile probe is not run in `--generate` mode (where the keyfile
     does not yet exist on disk). Backstop: do not provide a
     `CryptsetupTestKeyFile` mock either, so an accidental probe
     would also surface as a `MissingMock` error.
   - `dry_run_probe_error_propagates`: use at least two present LUKS
     candidates. Mock the first candidate's `CryptsetupTestKeyFile` to
     exit 0 (`Authenticated`) and the second to exit 5 (busy). Assert
     `plan_enroll` returns `Err(EnrollKeyFileError::...)` AND that
     `EnrollPlanReport.notes` contains, in order: any discovery notes
     accumulated before the probe loop, then the first disk's
     `PreviewNote::PerDisk { level: Skip, message: "keyfile already
     enrolled" }`. This pins that probe-derived skip notes from
     successful candidates earlier in the loop survive the mid-loop
     error -- without this assertion, an implementation that drops
     probe-derived notes on error (preserving only discovery notes)
     would still pass.
   - `real_run_does_not_probe_before_passphrase` (regression for the
     Phase-1 reviewer's High finding): real-run path
     (`dry_run = false`). Configure `MockRunner` using the existing
     enroll-test helpers (the patterns already in
     `enroll_key_file.rs` tests): `luks_uuid_ok`,
     `.with_luks_dump_text_luks2(...)`, `.with_mapper_closed(...)`,
     and `test_passphrase_fail` for the wrong-passphrase outcome on
     the first candidate's `CryptsetupTestPassphrase`. Provide **no**
     `CryptsetupTestKeyFile` mock at all. Two complementary assertions:
       1. Primary regression pin: assert
          `runner.requests()` (now exposed -- see `cli/src/cmd.rs`
          `MockRunner::requests`) contains zero
          `CmdRequest::CryptsetupTestKeyFile` entries. This is the
          direct expression of the property we want to pin: the
          dry-run probe is not hoisted into the real-run path.
       2. Backstop: assert the call returns the wrong-passphrase
          `EnrollKeyFileError::Validation` (and not a `MissingMock`
          error). Omitting the keyfile mock means an accidental
          probe attempt would surface as a different error variant,
          giving a second line of defense if `requests()` semantics
          ever change.
     Implementation note: before writing this test, scan existing
     `#[cfg(test)] mod tests` for the helper signatures and copy
     from the closest existing case rather than composing from this
     description.
2. **`just test-rust`** -- existing enroll tests, including any
   `compile_enroll_steps` golden/integration test, must still pass.
3. **`just test-vm braid-enroll`** -- runs `tests/cli/braid-enroll.py`.
   Test 1b must be updated per the "Critical files" entry; Tests 1, 2,
   3, 4 must keep passing unmodified.
4. **Manual sanity** in a VM: `braid enroll --dry-run` on a pool with
   1-of-2 disks already enrolled. Expect one enroll+backup pair of
   steps and one `[skip] disk <name>: keyfile already enrolled` line
   in the preview (Bracketed PerDiskStyle). Then run `braid enroll`
   and confirm the real run touches exactly the disk dry-run
   predicted.

## Out of scope

- Slot-1 availability preflight in dry-run. That is a separate fidelity
  gap (slot 1 occupied -> real run errors, dry-run does not foresee).
  It needs its own probe and decision; this fix does not address it.
- Reordering or refactoring `plan_enrollment`. The execute-time probe
  loop is left exactly as it stands.
