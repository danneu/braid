# Hoist enroll keyfile-probe trio into a shared helper

## Context

`cli/src/enroll_key_file.rs` has two near-duplicate blocks that emit the
`[wait] keyfile: checking against {name}...` -> `verify_key_file` ->
`[ok]   keyfile: already enrolled on {name}` / `[skip] keyfile: not yet
enrolled on {name}` trio:

- `enroll_key_file.rs:188-225` -- real-run, `plan_enrollment` inside the
  `EnrollmentPlanMode::ExistingKeyfile` arm.
- `enroll_key_file.rs:594-633` -- dry-run, `plan_enroll` inside the
  `dry_run && !generate` block.

The shared sandwich is ~16 lines per site with byte-identical wording,
already pinned by `tests/cli/braid-enroll.py:68-69, 103-115, 165-170,
218-219, 330-331, 390-393, 419-420`. Both sites also bypass the
codebase-wide `emit_status(...)`
convention (used in `add.rs`, `replace.rs`, `pool.rs`, `mapper_close.rs`)
by writing the ok/skip line via raw `eprint!("{}", status_line(...))`,
which means those rows aren't visible to the `status_tag::testing::capture_with`
test seam.

The fix is to hoist the trio into a single `probe_keyfile_enrollment`
helper next to the existing `verify_credential_for_targets` in
`cli/src/credential_verify.rs`. The two helpers are siblings: one is
*verify-and-abort* (any rejection is fatal), the other is
*probe-and-classify* (rejection is "not yet enrolled, continue"). They
share the wait/ok rendering primitives but differ in control flow and
wording (`accepted by` vs `already enrolled on`).

The third `verify_key_file` call site at `recover.rs:1973`
(`ensure_keyfile_enrolled`) is intentionally out of scope -- it emits no
status rows and has a verify-and-conditionally-enroll shape, not a
probe-and-classify shape.

Principle 13 (`docs/principles.md:69-113`) requires every `[wait]` row
to be closed by `[ok] | [skip] | [warn] | [fail] | error propagation`.
Both arms close their wait today; the helper preserves that.

## Recommended approach

### 1. New helper in `cli/src/credential_verify.rs`

Add (next to `verify_credential_for_targets` at `credential_verify.rs:30-73`):

```rust
/// Probe whether a candidate disk already has the keyfile installed.
///
/// Sibling to `verify_credential_for_targets`: same wait-line idiom,
/// but rejection is informational ("not yet enrolled") rather than
/// fatal. Emits exactly one `[wait]` row, then exactly one closer
/// (`[ok]` on Authenticated, `[skip]` on Rejected). On `LuksError` the
/// wait closes via the caller's error propagation per Principle 13.
///
/// Used by `braid enroll`'s dry-run preview (`plan_enroll`) and
/// real-run planner (`plan_enrollment`, ExistingKeyfile mode) so both
/// paths render byte-identical rows.
pub fn probe_keyfile_enrollment<R: CommandRunner>(
    runner: &R,
    target: &CredentialVerifyTarget,
    key_file_path: &Path,
    color_enabled: bool,
    mut emit: impl FnMut(&str),
) -> Result<VerifyOutcome, LuksError> {
    emit(&credential_wait_line(CredentialKind::KeyFile, color_enabled, &target.name));
    let outcome = luks::verify_key_file(runner, &target.device, key_file_path)?;
    let body = match outcome {
        VerifyOutcome::Authenticated =>
            format!("keyfile: already enrolled on {}", target.name),
        VerifyOutcome::Rejected =>
            format!("keyfile: not yet enrolled on {}", target.name),
    };
    let tag = match outcome {
        VerifyOutcome::Authenticated => StatusTag::Ok,
        VerifyOutcome::Rejected => StatusTag::Skip,
    };
    emit(&status_line(tag, color_enabled, &body));
    Ok(outcome)
}
```

Decisions:

- **Single target, not a slice.** Each call site already loops over
  candidates and interleaves per-candidate work (slot-1 preflight in
  real-run, note-pushing in dry-run) between probes. A slice helper
  would force callers to do a second loop over outcomes.
- **Take `&CredentialVerifyTarget`**, not `&str`+`&str` -- mirrors
  `verify_credential_for_targets`. Both call sites already have the
  `name` and `by_id.0` adjacent, so building a `CredentialVerifyTarget`
  per candidate is one line.
- **Take `&Path`**, not `Credential<'_>`. The helper is keyfile-specific
  by design -- the wording has no passphrase analogue. Accepting
  `Credential::Passphrase` would be a runtime panic surface.
- **Return `Result<VerifyOutcome, LuksError>`**, not
  `Result<_, EnrollKeyFileError>`. Keeps `credential_verify.rs` free of
  `EnrollKeyFileError`. Call sites convert via `?` because
  `From<LuksError> for EnrollKeyFileError` exists at
  `enroll_key_file.rs:29`.
- **Inline the format strings**, no new `status_tag.rs` helpers.
  Centralizing the wording in the helper is enough; the helper's own
  unit tests byte-pin the strings. Adding `keyfile_already_enrolled_line`
  / `keyfile_not_yet_enrolled_line` to `status_tag.rs` for one caller is
  over-engineering.
- **Pass `color_enabled` as a parameter**, don't fetch internally.
  Matches `verify_credential_for_targets` and avoids breaking the
  `cfg(test)`-only color-override seam in `status_tag.rs:164`.
- **Pass `emit` as a closure** (`impl FnMut(&str)`), not direct
  `emit_status`. Mirrors `verify_credential_for_targets`'s seam pattern.
  Production call sites pass `emit_status` (which is a function pointer
  with no captures, so no aliasing with `notes.push` after the call).

### 2. Mechanical edits at each call site

**`enroll_key_file.rs:188-225`** (real-run `ExistingKeyfile` arm):

Replace the `if let EnrollmentPlanMode::ExistingKeyfile = mode { ... }`
block with:

```rust
if let EnrollmentPlanMode::ExistingKeyfile = mode {
    let target = CredentialVerifyTarget {
        name: name.clone(),
        device: by_id.0.clone(),
    };
    if matches!(
        probe_keyfile_enrollment(
            runner, &target, key_file_path, color_enabled, emit_status,
        )?,
        VerifyOutcome::Authenticated
    ) {
        plan.push(DiskEnrollAction::AlreadyEnrolled {
            name: name.clone(),
            by_id: by_id.clone(),
        });
        continue;
    }
}
```

The fall-through `check_slot_one_available` + `eprintln!("enroll: ...")`
+ `NeedsEnroll` push at lines 227-233 is unchanged.

**`enroll_key_file.rs:594-633`** (dry-run `dry_run && !generate` block):

Replace the per-candidate `match luks::verify_key_file(...)` with:

```rust
let target = CredentialVerifyTarget {
    name: name.clone(),
    device: by_id.0.clone(),
};
match probe_keyfile_enrollment(
    runner, &target, key_file_path, color_enabled, emit_status,
) {
    Ok(VerifyOutcome::Authenticated) => {
        notes.push(PreviewNote::PerDisk {
            name: name.clone(),
            level: NoteLevel::Skip,
            message: "keyfile already enrolled".into(),
        });
    }
    Ok(VerifyOutcome::Rejected) => {
        needs_enroll.push((name.clone(), by_id.clone()));
    }
    Err(e) => {
        return EnrollPlanReport {
            notes,
            result: Err(e.into()),
        };
    }
}
```

**Imports in `enroll_key_file.rs`:**

- Add `probe_keyfile_enrollment` and `CredentialVerifyTarget` to the
  `crate::credential_verify::` use at lines 3-5.
- Add `emit_status` to the `crate::status_tag::` use at lines 13-15.
- Drop `emit_credential_wait_line` (no longer called directly here) and
  let the compiler flag any unused `status_line` / `StatusTag` /
  `CredentialKind` imports.

### 3. Unit tests

Two layers, both required:

**(a) Helper-level tests in `credential_verify.rs#tests`** (mirror
`verify_credential_for_targets_*` at `credential_verify.rs:201-324`,
reuse `key_file_runner` at `credential_verify.rs:146`):

- `probe_keyfile_enrollment_authenticated_emits_wait_then_already_enrolled`
  -- exit 0, both color modes; assert `emits ==
  [wait_line, status_line(Ok, "keyfile: already enrolled on disk1")]`
  and result is `Ok(VerifyOutcome::Authenticated)`.
- `probe_keyfile_enrollment_rejected_emits_wait_then_not_yet_enrolled`
  -- exit 2; assert emits == `[wait_line, status_line(Skip, "keyfile:
  not yet enrolled on disk1")]` and result is `Ok(Rejected)`. Pin that
  the `[skip]` row, not error propagation, closes the wait on
  rejection.
- `probe_keyfile_enrollment_luks_error_emits_wait_only_and_propagates`
  -- exit 5; assert emits is `[wait_line]` only and result is
  `Err(LuksError::OpenFailed { exit_code: 5, .. })`. Wait closes via
  caller's `?` per Principle 13.

These pin the helper's row-emission contract and the byte-exact
wording (`"keyfile: already enrolled on {name}"` / `"keyfile: not
yet enrolled on {name}"`).

**(b) Call-site tests in `enroll_key_file.rs#tests`** (essential --
without these, the call sites can silently regress to raw `eprint!`
and bypass `emit_status` while the helper tests still pass):

- `plan_enrollment_existing_keyfile_emits_keyfile_probe_rows_via_emit_status`
  -- real-run; two-disk scenario (one already enrolled, one not yet
  enrolled). Wrap the `plan_enrollment(..., EnrollmentPlanMode::ExistingKeyfile)`
  call in `crate::status_tag::testing::capture_with_color(false,
  || { ... })`, then assert the captured buffer contains
  `"[wait] keyfile: checking against {name}...\n"` and the matching
  `"[ok]   keyfile: already enrolled on {name}\n"` /
  `"[skip] keyfile: not yet enrolled on {name}\n"` for each disk in
  order. Also assert `<wait>.find < <ok-or-skip>.find` for each pair
  to pin the order.
- `plan_enroll_dry_run_emits_keyfile_probe_rows_via_emit_status`
  -- dry-run; same two-disk scenario, wrap `plan_enroll(..., dry_run=true,
  generate=false, ...)` in `capture_with_color(false, || { ... })` and
  assert the same row sequence in the captured buffer.

These two tests are what catches an accidental regression to raw
`eprint!` -- `capture_with_color` only sees lines that flowed through
`emit_status`, so a broken call site produces an empty buffer or
missing rows, while the existing `PreviewNote`-shape tests would
still pass.

The existing `enroll_key_file.rs#tests` (lines 1491-1705) keep their
current shape (assertions on `PreviewNote` and rendered preview
strings) -- they are orthogonal to the row-emission contract. Add the
two new tests above; do not modify the others.

**Test preambles:** every new `#[test]` item must have a contiguous
`//` line-comment block directly above it, per
`docs/testing.md:11-22`:

```rust
// Intent: one-line statement of the behavior verified.
// Why it exists: the regression risk this protects against.
// Scenario: the concrete sequence the test models.
#[test]
fn the_test() { ... }
```

Apply this to all five new tests (three in `credential_verify.rs`,
two in `enroll_key_file.rs`).

## Critical files to modify

- `cli/src/credential_verify.rs` -- add helper + tests.
- `cli/src/enroll_key_file.rs` -- replace two duplicate blocks, adjust
  imports.

## Reused existing utilities

- `crate::credential_verify::CredentialVerifyTarget`
  (`credential_verify.rs:7-11`) -- target newtype.
- `crate::luks::{verify_key_file, VerifyOutcome, LuksError}`
  (`luks.rs:870`, `luks.rs:523`) -- the underlying probe.
- `crate::status_tag::{credential_wait_line, status_line, emit_status,
  StatusTag, CredentialKind}` (`status_tag.rs:76-86, 58-64, 66-74,
  13-19, 22-25`) -- row rendering primitives.
- `From<LuksError> for EnrollKeyFileError` (`enroll_key_file.rs:29`)
  -- error conversion at the dry-run / real-run call sites.

## Verification

1. `just test-rust` -- the new `credential_verify.rs` tests pin the
   helper's row contract; the new `enroll_key_file.rs` `..._via_emit_status`
   tests pin that the call sites route through `emit_status` (without
   these, a regression to raw `eprint!` would not be caught by Rust
   tests). Existing `enroll_key_file.rs` tests at lines 1491-1705
   pass unchanged.
2. `just test-vm braid-enroll` -- end-to-end pin of the byte-identical
   wording. The Python tests at `tests/cli/braid-enroll.py:68-69,
   103-115, 165-170, 218-219, 330-331, 390-393, 419-420` must pass
   without modification.
3. `cargo clippy --all-targets -- -D warnings` -- catch any imports
   left dangling in `enroll_key_file.rs` after the refactor.

## Out of scope

- `recover.rs:1973` (`ensure_keyfile_enrolled`) -- different shape
  (no status rows, verify-and-conditionally-enroll). Leave alone.
- Any wording changes. The two strings are pinned by VM tests and must
  stay byte-identical.
- Modifying the existing `enroll_key_file.rs#tests` at lines
  1491-1705. They keep their current shape (assert on `PreviewNote`
  and rendered preview strings) and continue to pass unchanged. The
  two new `..._via_emit_status` call-site tests described in section
  3(b) are added alongside them and own the row-emission contract.
