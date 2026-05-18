# Fix: `braid enroll --generate` partial-apply recovery hints

## Context

When `braid enroll DIR --generate` runs, the order is:

1. `plan_enrollment` (passphrase verify + slot-1 preflight on every disk).
2. `generate_key_file` creates `DIR/braid.key`.
3. `apply_enrollment` loops the plan calling `luks::enroll_key_file` +
   `backup_luks_header_post_mutation` per disk.

If step 3 fails mid-loop (transient EBUSY on disk2, SIGINT, suspend
during Argon2, etc.), the user is left with:

- The freshly-generated keyfile on the USB.
- disk1 fully enrolled (slot 1 holds the new key).
- disk2 untouched (or partially -- usually slot 1 still empty thanks to
  `luksAddKey`'s atomic-per-call behavior).

The natural retry is what the manual documents as the only `--generate`
example: `sudo braid enroll /mnt/usb --generate`. That command refuses
with the validation error from `validate_key_file_path`:

> braid.key already exists at /mnt/usb/braid.key; remove it manually if
> you want to generate a new one

This wording actively misleads: if the user follows it (remove the
keyfile and re-run `--generate`), the new keyfile will not match the
key already enrolled in disk1's slot 1, leaving disk1 holding an
orphaned key the new keyfile cannot unlock. Slot-1 preflight in
`GenerateNew` mode will then refuse disk1 with the `luksKillSlot`
remediation, forcing the user to hand-prune slot 1 before retrying.

The correct recovery is to drop `--generate` and re-run
`sudo braid enroll /mnt/usb`. In `ExistingKeyfile` mode,
`plan_single_disk_enrollment` probes each candidate and classifies
disk1 as `AlreadyEnrolled` (idempotent skip) and disk2 as `NeedsEnroll`,
finishing the operation cleanly. Today's CLI never tells the user this.

Goal: surface the correct recovery action at both moments where the
user can act on it -- at the apply failure (in-band), and at the naive
retry (the misleading validation error).

`braid recover` and pending-op journals are intentionally not in scope:
Principle 3 names `add`/`remove`/`remove-missing`/`replace` as the
journal-writing set, and `enroll` doesn't change pool membership. The
fix is UX wording, not a recovery-model change.

## Critical files

- `cli/src/enroll_key_file.rs`
  - `EnrollPlan::execute` (lines 450-493): the seam where `self.generate`
    is known and `apply_enrollment` is called. Wrap its `Err` here so
    the hint is gated on `--generate`.
  - `validate_key_file_path` (lines 499-514): the "braid.key already
    exists" message. Enrich its wording.
- `cli/src/test_fixtures/enroll_key_file.rs`
  - Add `enroll_add_keyfile_fail(device, key_file, passphrase, ...)`
    sibling to existing `enroll_add_keyfile_ok` (line 263). Reuses
    `crate::test_fixtures::err_raw` (already imported as
    `enroll_err_raw` in the enroll test module).
- `tests/cli/braid-enroll-generate.py`
  - Test 3 (`--generate refuses overwrite`, line 109): extend the
    `machine.fail(...)` assertion to capture stderr and check for the
    new hint substring.
- `manual/commands/enroll.md`
  - Line 60 (Step 2 of "What happens under the hood"): replace
    "refuses if it does -- remove it manually first" with wording that
    names the drop-`--generate` recovery path.
  - Line 78 (Safety checks): same update to the `--generate refuses
    if braid.key already exists` bullet.

## Design

### 1. Apply-time hint (gated on `--generate`)

In `EnrollPlan::execute`, after `apply_enrollment(...)`, map any `Err`
into an enriched error when `self.generate` is true. Derive the DIR
via `key_file_directory(params.key_file_path)` (the helper at
`cli/src/enroll_key_file.rs:516`) -- the same normalizer the
retry-time hint uses. Raw `Path::parent()` would render an empty
string for a relative `braid.key` argument and produce a broken
`"braid enroll "` recovery command; routing both hint sites through
`key_file_directory` keeps them consistent.

Sketch:

```rust
if self.generate {
    generate_key_file(params.key_file_path)?;
    eprintln!("ok: generated {}", params.key_file_path.display());
}

let apply_result =
    apply_enrollment(runner, &enrollment, &passphrase, params.key_file_path, params.paths);

match apply_result {
    Ok(()) => Ok(()),
    Err(e) if self.generate => Err(EnrollKeyFileError::Validation(
        partial_generate_recovery_message(&e, params.key_file_path),
    )),
    Err(e) => Err(e),
}
```

`partial_generate_recovery_message` calls
`key_file_directory(key_file_path)` internally for the DIR rendering
so the apply-time and retry-time hints stay byte-for-byte consistent
on the recovery command. It follows the precedent of
`header_backup_failure_message` in `cli/src/luks.rs:539-545` -- keep
the underlying error verbatim and append the recovery command.

Proposed wording:

> {underlying}
>
> The keyfile at {path} was generated but enrollment did not complete
> on every disk. To finish, drop `--generate` and re-run:
>   braid enroll {dir}

Use `--` (ASCII) per CLAUDE.md and the project's CLI output style rule
(AGENTS.md "CLI Output Style").

### 2. Retry-time hint (`braid.key already exists`)

In `validate_key_file_path`, extend the `generate && exists` branch with
the recovery hint. Both legitimate user actions remain on the page;
order them with the "interrupted-run" recovery first since that's the
likelier intent on a populated USB.

Proposed wording:

> braid.key already exists at {path}.
> If a prior `--generate` run was interrupted, drop `--generate` and
> re-run `braid enroll {dir}` to finish enrolling the existing keyfile.
> Otherwise remove it manually if you want to generate a new one.

`{dir}` comes from `key_file_path.parent()` (use `key_file_directory`
helper already in the file at line 516).

### 3. Tests

#### Rust unit test: apply-time hint

Add a new test in the `tests` mod of `cli/src/enroll_key_file.rs`,
parallel to `apply_enrollment_returns_enriched_error_when_backup_fails`
(line 2877). Drive `EnrollPlan::execute` directly (not
`apply_enrollment`) because the hint is gated on `self.generate`.

Preamble (per AGENTS.md Test Conventions):

```rust
// Intent: EnrollPlan::execute enriches the apply-phase error with the
//   drop-`--generate` recovery hint when --generate succeeded.
// Why it exists: a partial luksAddKey failure on disk2 leaves an orphan
//   keyfile on the USB; without the hint, the user's documented retry
//   command (`braid enroll DIR --generate`) gets the misleading
//   `braid.key already exists; remove it manually` validation error and
//   removing+regen orphans disk1's slot 1.
// Scenario: 2-disk plan from plan_enrollment, disk1 enroll + backup ok,
//   disk2 luksAddKey returns nonzero. Assert error wording.
```

`EnrollPlan::execute` consumes `self.candidates` and re-runs
`plan_enrollment` against them at line 471 -- it does NOT replay
`self.steps`. The test must therefore mock the full
`plan_enrollment` -> `generate_key_file` -> `apply_enrollment`
sequence, not just the apply phase.

Recipe (concrete -- every mock the runner will see, in order):

1. **Tempdir + non-existing keyfile path.** `let tmp =
   tempfile::tempdir().unwrap(); let kf = tmp.path().join("braid.key");
   let kf_str = kf.display().to_string();`
   The file must NOT exist before `execute` runs so
   `generate_key_file` creates it; do not use
   `enroll_make_existing_keyfile`. Pass `&kf` (a `&Path`) to
   `EnrollKeyFileParams::key_file_path` and `&kf_str` (a `&str`) to
   the `enroll_add_keyfile_ok` / `enroll_add_keyfile_fail` mock
   composers -- the latter take `&str` per
   `cli/src/test_fixtures/enroll_key_file.rs:263`.
2. **Passphrase source via file.** Write the passphrase to
   `tmp.path().join("pass")` and pass it through
   `EnrollKeyFileParams { passphrase_file: Some(&pass_path),
   passphrase_stdin: false, .. }`. Avoids the stdin/TTY path inside
   `luks::read_passphrase`.
3. **Candidates and EnrollPlan.** Construct the plan directly:
   `EnrollPlan { notes: vec![], steps: vec![], candidates: vec![
   (disk("disk1"), enroll_by_id(d1)), (disk("disk2"), enroll_by_id(d2))
   ], generate: true }`. `steps` is unused on the execute path; leaving
   it empty is fine and avoids redundant fixture work.
4. **Mocks (in the order `plan_enrollment` then `apply_enrollment`
   issue them):**
   - `verify_credential_for_targets` (Passphrase variant) issues
     `CryptsetupTestPassphrase { device }` once per candidate. Use
     `enroll_test_passphrase_ok(d1, pass)` and
     `enroll_test_passphrase_ok(d2, pass)` (already in
     `test_fixtures/enroll_key_file.rs`).
   - `plan_single_disk_enrollment` in `GenerateNew` mode skips the
     keyfile probe and goes straight to `check_slot_one_available`,
     which issues `CryptsetupLuksDump { device }`. Use
     `enroll_luks_dump_slot1_empty(d1)` and
     `enroll_luks_dump_slot1_empty(d2)`.
   - `apply_enrollment` issues, in order:
     - `enroll_add_keyfile_ok(d1, &kf_str, pass)` -- disk1 luksAddKey
     - `CryptsetupLuksHeaderBackup { device: d1, backup_path:
       <paths.luks_headers_dir>/braid-disk1.luksheader.tmp }` ->
       `mock_ok("cryptsetup luksHeaderBackup", "")` (mirrors the
       backup mock recipe in `apply_enrolls_needs_enroll_items` at
       line 2719).
     - `enroll_add_keyfile_fail(d2, &kf_str, pass)` -- the new failing
       sibling (see fixture addition below).
5. **Assertions.** Call `plan.execute(&runner, &params)`; expect
   `Err`. The error string must contain:
   - The underlying error fragment, e.g. `"cryptsetup luksAddKey
     failed"` (per `luks::enroll_key_file`'s wording at
     `cli/src/luks.rs:1027-1031`).
   - The recovery hint markers: ``"drop `--generate`"`` and
     `"braid enroll "` followed by the DIR (`tmp.path().display()`).

**Fixture addition.** Add `enroll_add_keyfile_fail` to
`cli/src/test_fixtures/enroll_key_file.rs` next to
`enroll_add_keyfile_ok` (line 263). The module already imports
`err_raw` directly from `super::mount` at line 58, so the body uses
the bare `err_raw(...)` -- not `shared::err_raw(...)`. Do not add a
new import:

```rust
pub(crate) fn enroll_add_keyfile_fail(
    device: &str,
    key_file: &str,
    passphrase: &str,
) -> (CmdRequest, Vec<u8>, RawCommandOutput) {
    (
        CmdRequest::CryptsetupLuksAddKeyFile {
            device: device.to_owned(),
            key_file_path: key_file.to_owned(),
        },
        passphrase.as_bytes().to_vec(),
        err_raw(
            "cryptsetup luksAddKey",
            5,
            "Device or resource busy",
        ),
    )
}
```

Re-export it through `cli/src/test_fixtures/mod.rs` alongside the
other `enroll_*` helpers so the existing
`use crate::test_fixtures::{ enroll_add_keyfile_ok, ... }` import in
the enroll test module can pick it up by adding one name.

**Cross-check.** Mock ordering and missing-mock errors are runtime
`MockRunner` panics, so `cargo check` cannot surface them -- it only
compiles. Use:

- `cargo check -p braid-cli --tests` while adding the fixture, to
  catch type / import errors fast (e.g. the bare `err_raw` vs
  `shared::err_raw` distinction, `&str` vs `PathBuf` mismatches).
- `cargo test -p braid-cli execute_generate_partial_failure --
  --nocapture` to actually run the test under `MockRunner`. Each
  missing mock prints the unmatched `CmdRequest` in the panic
  message, naming the fixture the recipe is missing.

The same `cargo test` command is the "fails before the fix / passes
after" probe listed in Verification.

#### Rust unit test: retry-time hint

Update or extend the existing assertion at line 1396-1399:

```rust
assert!(
    err.to_string().contains("braid.key already exists"),
    "unexpected error: {err}"
);
```

Add a second assertion in the same test (or a sibling test) that the
hint substring (`drop \`--generate\``, `braid enroll`) is present.
Keeps the test compact.

#### VM test extension

`tests/cli/braid-enroll-generate.py` Test 3 (line 109):

Replace the bare `machine.fail(...)` with a capture-and-check, mirroring
how Test 5b in `braid-enroll.py` uses `machine.execute(...)`:

```python
with subtest("Test 3: --generate refuses overwrite"):
    pq = shlex.quote(passphrase)
    status, output = machine.execute(
        f"printf '%s\\n' {pq} | braid enroll /tmp/usb --generate --passphrase-stdin 2>&1"
    )
    assert status != 0, f"expected nonzero exit; got status={status}, output={output!r}"
    assert "braid.key already exists" in output, (
        f"expected `already exists` error, got: {output!r}"
    )
    assert "drop `--generate`" in output, (
        f"expected interrupted-run recovery hint, got: {output!r}"
    )
    assert "braid enroll /tmp/usb" in output, (
        f"expected recovery command pointing at DIR, got: {output!r}"
    )
```

### 4. Manual update

Update `manual/commands/enroll.md` in the same change so the user
guide matches the in-band hint wording.

- Step 2 (line 60): replace the parenthetical "refuses if it does --
  remove it manually first" with something like "refuses if it does
  -- if a prior `--generate` run was interrupted, drop `--generate`
  and re-run to finish enrolling the existing keyfile; otherwise
  remove it manually first".
- Safety checks (line 78): replace the bare "refuses if `braid.key`
  already exists at the target path" with the same recovery-aware
  wording.

Keep the cookbook tone (brief, copy-paste examples) per AGENTS.md's
"User Guide" section. No other manual files need changes -- the
guides/auto-unlock.md flow describes the happy path only and does not
mention partial recovery.

## Out of scope

- Non-generate apply failures (retry is the same command; obvious to
  users; the existing finding is specifically about `--generate`).
- Adding a pending-op journal for `enroll` (intentionally outside
  Principle 3's mutating-command set; `braid recover` is membership-
  scoped).
- Probing whether the existing `braid.key` actually authenticates
  against slot 1 (would require a passphrase at validation time, much
  larger change for marginal benefit -- the text hint is sufficient).
- Updating the apply_enrollment loop to track partial progress (the
  hint already conveys "not all disks done"; the recovery path is
  idempotent and self-correcting).

## Verification

Run in order:

1. `just test-rust` -- new unit tests pass; the existing apply-failure
   and validation tests still pass with the augmented assertions.
2. `just test-vm braid-enroll-generate` -- VM Test 3's new hint
   assertions pass.
3. `just test-vm braid-enroll` -- regression check on the sibling
   enroll VM test (no expected behavior change, but the validation
   message wording is shared infrastructure).

Manual sanity check (no VM required):

- Run `cargo test -p braid-cli execute_generate_partial_failure` and
  confirm the new test fails before the fix and passes after.
- `grep -rn 'remove it manually' cli/src/ manual/ tests/` -- every
  match must point at the recovery hint (drop `--generate`, re-run);
  no bare "remove it manually" wording should survive in source,
  manual, or tests.
- `grep -rn '\<braid\.key already exists' cli/src/ manual/ tests/` --
  every match must be followed by the drop-`--generate` recovery
  hint, both in the source message and in the manual.
