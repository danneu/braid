# Plan: enroll dry-run slot-1 conflict surfaces in preview

## Context

`braid enroll --generate --dry-run` (and `braid enroll --dry-run` against
a `Rejected` keyfile probe) currently previews an `enroll keyfile ->
LUKS slot 1 on <disk>` step on disks whose slot 1 is already occupied
by an unknown key. The real run, behind the passphrase prompt, refuses
with the canonical `slot 1 on <disk> is occupied by an unknown key.
Remove it first with cryptsetup luksKillSlot ...` error.

This violates the dry-run preview-fidelity contract that
[`docs/decisions/022-dry-run-preview-model.md`](../../docs/decisions/022-dry-run-preview-model.md)
treats as the migration's whole purpose. The slot check is
`cryptsetup luksDump` -- passphrase-free, side-effect-free, parsing
only -- and is well inside the dry-run probe envelope authorized by
[`docs/decisions/012-intent-cli.md:52`](../../docs/decisions/012-intent-cli.md):
"side-effect-free, passphrase-free LUKS probes only."

Root cause is structural, not arithmetic: `plan_enroll`'s dry-run arm
inlines a half-classifier (keyfile probe + Skip note) while a complete
per-disk classifier already exists in
[`plan_single_disk_enrollment`](../../cli/src/enroll_key_file.rs)
(`cli/src/enroll_key_file.rs:192-228`), which `add`/`replace` already
route through and which encapsulates the probe + slot-1 check + canonical
error text. Both dry-run paths (`--generate` and existing-keyfile) bypass
that helper today. The fix unifies them.

## Files modified

- `cli/src/enroll_key_file.rs` -- replace the inlined dry-run probe arm
  in `plan_enroll` with a per-candidate call to
  `plan_single_disk_enrollment`; rewrite the `plan_enroll` boundary doc
  comment to match the new behavior; update four unit tests to add
  slot-1 dump mocks for newly-classified disks; add two new dry-run
  conflict tests.
- `tests/cli/braid-enroll-generate.py` -- add a subtest pinning the
  dry-run preview shape under a slot-1 conflict; clear disk2 slot 1
  after the conflict assertions so Test 5 still observes a clean
  `PresentLuks` survivor.

No new files. No public API changes.

## Implementation

### 1. Delegate the dry-run loop to the shared helper

`cli/src/enroll_key_file.rs:610-643` -- replace the `dry_run && !generate`
arm and the `else` arm with a single `if dry_run` arm that loops through
`plan_single_disk_enrollment` for every candidate, and the existing
`else` arm (real-run) untouched:

```rust
let steps = if dry_run {
    let mode = if generate {
        EnrollmentPlanMode::GenerateNew
    } else {
        EnrollmentPlanMode::ExistingKeyfile
    };
    let mut needs_enroll: Vec<EnrollmentCandidate> = Vec::with_capacity(candidates.len());
    for (name, by_id) in &candidates {
        match plan_single_disk_enrollment(runner, name, by_id, key_file_path, mode) {
            Ok(DiskEnrollAction::AlreadyEnrolled { name, .. }) => {
                notes.push(PreviewNote::PerDisk {
                    name: name.as_str().to_owned(),
                    level: NoteLevel::Skip,
                    message: "keyfile already enrolled".into(),
                });
            }
            Ok(DiskEnrollAction::NeedsEnroll { name, by_id }) => {
                needs_enroll.push((name, by_id));
            }
            Err(e) => {
                return Err(PlanFailure::with_notes(notes, e));
            }
        }
    }
    compile_enroll_steps(&needs_enroll, key_file_path, generate, paths)
} else {
    compile_enroll_steps(&candidates, key_file_path, generate, paths)
};
```

Why this shape (not "add `check_slot_one_available` inline"):

- `plan_single_disk_enrollment` already returns
  `Err(EnrollKeyFileError::Validation("slot 1 on <name> ... is occupied
  by an unknown key. Remove it first with `cryptsetup luksKillSlot ...`."))`
  -- byte-for-byte the same wording the real run produces. Inline calls
  would duplicate that error string.
- Routing through the helper also closes the existing-keyfile dry-run gap
  the finding flags: a `Rejected` probe today pushes to `needs_enroll`
  with no slot-1 check; under the new code, `plan_single_disk_enrollment`
  runs the slot-1 check after a `Rejected` probe in `ExistingKeyfile`
  mode, so the preview matches what the real run would do.
- The `[wait]/[ok]/[skip]` rows pinned by
  `plan_enroll_dry_run_emits_keyfile_probe_rows_via_emit_status` are
  preserved -- `plan_single_disk_enrollment` calls the same
  `probe_keyfile_enrollment(... emit_status, ...)` the inlined loop did.
- Bail-on-first-conflict matches `plan_enrollment`'s real-run loop
  (`cli/src/enroll_key_file.rs:273-279`), keeping dry-run and real-run
  diagnostic shapes identical.

Notes contract preserved: `PlanFailure::with_notes(notes, e)` carries
any `keyfile already enrolled` Skip notes accumulated from earlier
iterations to stderr via `preview::emit_notes_to_stderr` at
`cli/src/enroll_key_file.rs:669-676`. No new note rendering work.

No new `[wait]/[ok]` row emission for `check_slot_one_available`:
[`check_key_slot`](../../cli/src/luks.rs) at `cli/src/luks.rs:1037-1062`
runs only `cryptsetup luksDump` JSON parsing -- no Argon2 / KDF work --
so it falls outside the "Argon2-bounded probe" carve-out in
`docs/decisions/012-intent-cli.md:54`.

### 2. Rewrite the `plan_enroll` boundary doc comment

`cli/src/enroll_key_file.rs:561-577` -- the current "Dry-run keyfile
probe: when `dry_run && !generate`, ..." paragraph documents the old
behavior and contradicts the new `if dry_run` delegation. Replace it
with text that names the new contract:

```rust
/// Plan a `braid enroll` run. Owns the pending-op preflight,
/// keyfile-path validation, and pre-passphrase discovery. Per-disk
/// skip notes land on `EnrollPlan.notes` when discovery produces at
/// least one candidate, or on `PlanFailure::notes` when the
/// planner bails (e.g. no candidates, mid-loop probe error,
/// dry-run slot-1 conflict).
///
/// Dry-run classification: when `dry_run`, each candidate is routed
/// through `plan_single_disk_enrollment` so the preview matches the
/// real run's classification rules. `ExistingKeyfile` mode probes the
/// keyfile (passphrase-free, emits `[wait]/[ok]/[skip]` rows);
/// `Authenticated` becomes a `keyfile already enrolled` Skip note,
/// `Rejected` falls through to the slot-1 check. `GenerateNew` mode
/// (set by `--generate`) skips the keyfile probe entirely (the file
/// does not exist yet) and runs only the slot-1 check. Slot-1
/// conflicts surface as `PlanFailure` with the same canonical
/// `cryptsetup luksKillSlot` recovery wording the real run uses.
/// Real-run path (`dry_run = false`) leaves every discovered candidate
/// in the step list and defers classification to `plan_enrollment` at
/// execute time after the passphrase prompt.
```

The wording deliberately ties the dry-run behavior to the shared helper
so a future reader expanding either path is told to find the rules in
`plan_single_disk_enrollment`, not to duplicate them.

### 3. Update four existing dry-run unit tests

MockRunner is strict (`CmdError::MissingMock` at `cli/src/cmd.rs:1372`),
so tests that route a `Rejected`-probe or `--generate` candidate through
the new code must mock the `CryptsetupLuksDump` call:

- `dry_run_skips_already_enrolled_disks` (~line 1498) -- d2 is
  `Rejected`. Add `enroll_luks_dump_slot1_empty(d2)` to the runner. The
  existing "no enroll step for d1, enroll+backup step for d2" assertions
  remain valid.
- `plan_enroll_dry_run_emits_keyfile_probe_rows_via_emit_status` (~line
  1576) -- d2 is `Rejected`. Add `enroll_luks_dump_slot1_empty(d2)`.
  Existing wait/ok/skip row assertions remain valid since the row
  emitter is unchanged.
- `dry_run_with_generate_skips_probe` (~line 1707) -- `--generate`
  classifies every candidate via `GenerateNew` mode, which now triggers
  slot-1 dumps. Add `enroll_luks_dump_slot1_empty(d1)` and
  `enroll_luks_dump_slot1_empty(d2)`. The existing "zero
  `CryptsetupTestKeyFile` requests" assertion remains valid (the slot-1
  check is a separate request kind).
- `cmd_generate_dry_run_short_circuits` (~line 3232) -- `--generate`
  still short-circuits before passphrase/keyfile/mutation work, but the
  dry-run planner now runs the passphrase-free slot-1 inventory probe.
  Add `enroll_luks_dump_slot1_empty(d1)` and update the comment so the
  intended boundary is no passphrase, no keyfile probe, and no keyfile
  creation.

No change required for:
- `dry_run_all_already_enrolled_emits_zero_steps` -- both disks
  `Authenticated` so slot-1 check is skipped (per
  `plan_single_disk_enrollment`'s early return at line 215).
- `dry_run_probe_error_propagates` -- d2's probe error short-circuits
  inside `plan_single_disk_enrollment` before the slot-1 check.
- `generate_dry_run_rejects_plain_directory_without_key_creation` --
  bails at preflight validation before `plan_enroll` enters the loop.

### 4. Add new dry-run unit tests pinning the new contract

Two new unit tests in the `tests` mod alongside the existing dry-run
probe tests (~line 1170 region):

- `dry_run_with_generate_surfaces_slot1_conflict` -- 2-disk pool, d1
  slot 1 empty, d2 slot 1 occupied via `enroll_luks_dump_slot1_occupied(d2)`.
  Run `plan_enroll(... generate=true, dry_run=true ...)`. Assert
  `PlanFailure` whose error text contains `slot 1 on disk2` and
  `occupied by an unknown key`, and assert no `CryptsetupTestKeyFile`
  request issued (`--generate` skips the keyfile probe).
- `dry_run_existing_keyfile_surfaces_slot1_conflict` -- 2-disk pool, d1
  keyfile `Authenticated`, d2 keyfile `Rejected` + slot 1 occupied.
  Assert `PlanFailure` with the same canonical wording AND that
  `PlanFailure::notes` contains the d1 `keyfile already enrolled` Skip
  note (verifies note-preservation contract under the new error path).

Fixtures already exist: `enroll_luks_dump_slot1_empty`,
`enroll_luks_dump_slot1_occupied`, `enroll_test_keyfile_ok`,
`enroll_test_keyfile_fail`, `enroll_discovery_two_disks`,
`enroll_make_membership`, `enroll_make_existing_keyfile` --
all in `cli/src/test_fixtures/enroll_key_file.rs`.

### 5. Add VM-test subtest pinning end-to-end preview shape and clean up VM state

`tests/cli/braid-enroll-generate.py` has two changes that must happen
together:

**5a. Add Test 4b (dry-run conflict preview).** Reuses Test 4's
slot-1-conflict state (disk2 slot 1 holds the unknown key from Test 4's
`cryptsetup luksAddKey --key-slot 1`). Insert after Test 4's existing
real-run assertion at line 134-139, before any cleanup:

```python
with subtest("Test 4b: --generate --dry-run surfaces slot 1 conflict"):
    pq = shlex.quote(passphrase)
    machine.succeed(
        f"rc=0; printf '%s\\n' {pq} | "
        f"braid enroll /tmp/usb --generate --dry-run "
        f">/tmp/dr.out 2>/tmp/dr.err || rc=$?; echo $rc > /tmp/dr.rc"
    )
    rc = machine.succeed("cat /tmp/dr.rc").strip()
    out = machine.succeed("cat /tmp/dr.out")
    err = machine.succeed("cat /tmp/dr.err")
    assert rc != "0", f"dry-run must fail on slot-1 conflict; got rc={rc}"
    assert out == "", f"failed dry-run must leave stdout empty, got: {out!r}"
    assert "slot 1 on disk2" in err, f"expected slot-1 error, got: {err!r}"
    assert "occupied by an unknown key" in err, (
        f"expected canonical wording, got: {err!r}"
    )
    machine.fail("test -f /tmp/usb/braid.key")
```

**5b. Clean up disk2 slot 1 before Test 5.** Test 5 Phase A
(`tests/cli/braid-enroll-generate.py:200-203`) runs
`braid enroll /tmp/usb --generate --dry-run` and expects
`machine.succeed`, relying on disk2 being a clean `PresentLuks`
survivor. After this plan lands, that command will check slot 1 on
disk2, and Test 4's leftover unknown key would refuse it with the
canonical slot-1 conflict error, breaking Test 5.

After the Test 4b block, kill slot 1 on disk2 to restore the clean
`PresentLuks + empty slot 1` state Test 5 needs:

```python
# Clear the unknown key Test 4/4b planted in disk2 slot 1 so Test 5
# observes a clean PresentLuks survivor. Slot 1 must be empty for
# Test 5 Phase A's `--generate --dry-run` to render the mixed skip
# notes; otherwise the new slot-1 check would refuse on disk2.
machine.succeed("cryptsetup luksKillSlot --batch-mode /dev/disk/by-id/virtio-disk2 1")
```

Update Test 5's preamble comment (`tests/cli/braid-enroll-generate.py:164-171`,
the "Scenario" paragraph) to make the disk2 contract explicit AND fix
the pre-existing typo that conflates disk2 with the fabricated absent
entry (the test actually adds disk3 to pool.json, not disk2 -- see the
`jq` edit at `tests/cli/braid-enroll-generate.py:190-195` and the
disk3 bullet at `:183-185`):

> Scenario: after Test 4b restores disk2 slot 1 to empty, disk1 and
> disk2 are LUKS-formatted, with disk2 as a clean `PresentLuks`
> survivor (slot 0 holds the passphrase, slot 1 empty). We wipe
> disk1's LUKS header (`PresentNotLuks`) and add a third membership
> entry `disk3` to pool.json pointing at a fabricated by-id path
> that udev never populated (`Absent`). ...

Update Test 5's inline disk2 bullet (`tests/cli/braid-enroll-generate.py:178-182`)
to reflect "slot 1 empty" rather than leaving slot state implicit.
The disk1 and disk3 bullets are already correct.

Test 4 itself keeps its real-run assertion -- the dry-run case
augments coverage rather than replacing it.

### 6. Test preamble form

Per [`docs/testing.md:11`](../../docs/testing.md), every Rust test
preamble is a contiguous block of `//` line comments directly above
the test item, not a `/* ... */` block. Both new unit tests
(`dry_run_with_generate_surfaces_slot1_conflict`,
`dry_run_existing_keyfile_surfaces_slot1_conflict`) use the canonical
form:

```rust
// Intent: <one-line statement of the behavior verified>.
// Why it exists: <the regression risk this protects against>.
// Scenario: <concrete real-world sequence>.
#[test]
fn the_test() { ... }
```

Each new unit test gets its own Scenario tied to the specific bug path
it covers:

- `dry_run_with_generate_surfaces_slot1_conflict` -- "Operator runs
  `braid enroll /mnt/usb --generate --dry-run` against a 2-disk pool;
  an earlier troubleshooting session left an unknown key in disk2 slot
  1 via `cryptsetup luksAddKey --key-slot 1`. `--generate` skips the
  keyfile probe (file does not exist yet) and the new slot-1 check
  refuses with the canonical recovery hint instead of previewing a
  bogus enroll step."
- `dry_run_existing_keyfile_surfaces_slot1_conflict` -- "Operator runs
  `braid enroll /mnt/usb --dry-run` (CLI positional is a directory;
  `cli/src/main.rs:604` appends `braid.key`) against a 2-disk pool
  where disk1 already has the keyfile in slot 1 from a partial earlier
  run, and disk2 has a foreign key in slot 1 (e.g., a leftover from
  manual `luksAddKey` troubleshooting). The keyfile probe authenticates
  on disk1 (Skip note) and is rejected on disk2; the new slot-1 check
  refuses the rejected disk before the preview prints, with disk1's
  Skip note preserved on the failure-path stderr."

The new VM subtest (Test 4b) preamble follows the file's existing
`with subtest("...")` inline-comment convention; Test 5's preamble
already exists -- only its scenario paragraph and disk2 bullet need
the updates from step 5b.

## Verification

1. `just test-rust` -- all unit tests pass, including the four updated
   ones and the two new dry-run conflict tests.
2. `just test-vm braid-enroll-generate` -- VM test passes end-to-end:
   - Test 4 (real-run conflict refusal) still green.
   - New Test 4b (dry-run conflict preview) emits nonzero exit with the
     canonical slot-1 conflict message on stderr, empty stdout, and no
     `braid.key` created.
   - Test 5 Phase A still green: the slot-1 cleanup between Test 4b and
     Test 5 restores disk2 to the clean `PresentLuks + empty slot 1`
     state Test 5's mixed-skip-notes assertions require.
3. Manual sanity (VM only, optional): build the CLI, set up the same
   2-disk pool with disk2 slot 1 occupied via
   `cryptsetup luksAddKey --key-slot 1`, run
   `braid enroll /tmp/usb --generate --dry-run` and confirm:
   - stderr contains the canonical `slot 1 on disk2 ... cryptsetup
     luksKillSlot` recovery hint
   - stdout is empty (preserved-context failure path)
   - exit code is nonzero

## Out of scope

- No change to `plan_enrollment` (real-run helper) -- it already calls
  `check_slot_one_available` via `plan_single_disk_enrollment`.
- No change to `add`/`replace` `--enroll DIR` paths -- they already route
  through `plan_single_disk_enrollment` and surface slot-1 conflicts in
  their dry-run previews today.
- No new `[wait]/[ok]` row emission for the slot-1 dump -- not Argon2
  work; decision-012 only carves out the row contract for
  Argon2-bounded probes.
- No `PreviewNote::Warn` "slot 1 occupied -- skipping" alternative shape.
  Real-run bails the entire enrollment; dry-run mirrors that behavior to
  keep the two outputs equivalent per decision-022.
