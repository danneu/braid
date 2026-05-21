# Plan: clarify enroll preflight wording in manual

## Context

`manual/commands/enroll.md:64-67` describes the per-disk preflight as a
single numbered step:

```
6. For each disk, checks LUKS slot 1:
   - If the keyfile already works on this disk, reports "already enrolled" and skips.
   - If slot 1 is occupied by an unknown key, refuses with an error (...).
   - If slot 1 is free, proceeds.
```

This is structurally misleading. The implementation in
`cli/src/enroll_key_file.rs:192-228`
(`plan_single_disk_enrollment`) runs two distinct phases:

1. **Keyfile probe** (only in `ExistingKeyfile` mode, i.e. no
   `--generate`): runs `probe_keyfile_enrollment`. `Authenticated` ->
   "already enrolled", skip. `Rejected` -> fall through to phase 2. Any
   other non-zero exit (e.g. EBUSY) surfaces as a hard error -- this is
   the regression guard at `cli/src/enroll_key_file.rs:2500`
   (`plan_keyfile_verify_busy_surfaces_open_failed_not_proceeds`).
2. **Slot-1 occupancy check** (for disks not already classified as
   already enrolled -- i.e. those that fell through from a Rejected
   probe in `ExistingKeyfile` mode, plus every candidate in
   `GenerateNew` mode): runs `check_slot_one_available`. Empty ->
   proceed. Occupied -> refuse with the `cryptsetup luksKillSlot`
   remediation.

Two consequences of the current wording:

- The header "checks LUKS slot 1:" lumps both phases under a single
  slot-1 check, so an operator debugging an incorrect "already enrolled"
  report has no signal in the manual that a separate keyfile probe ran
  first.
- The first bullet ("If the keyfile already works on this disk") is
  actively wrong in `--generate` mode -- the keyfile does not exist yet
  and no probe runs (`plan_generate_new_skips_keyfile_probe` at
  `cli/src/enroll_key_file.rs:2564` is the paired regression test).

This is a docs-only pivot from a verify-issue finding. The finding's
proposed wording leaked implementation detail (`cryptsetup
test-passphrase exit 0`) into the user manual; we keep cookbook-style
wording instead and align with the sibling `discover.md` numbered-step
format.

## Scope

Single file edit: `manual/commands/enroll.md`. No code changes, no test
changes, no changes to sibling docs (the existing "Safety checks"
section in the same file already captures both invariants correctly via
"Slot 1 conflicts are detected before the keyfile is generated" and
"Idempotent: if the keyfile is already enrolled on a disk, that disk is
skipped"; auto-unlock guide and add/replace docs only mention "slot 1"
in passing and are not misleading).

## Change

Replace `manual/commands/enroll.md:64-67` (the current step 6) with two
distinct numbered steps, in flat cookbook style consistent with
`manual/commands/discover.md:55-71`:

```
6. **Without `--generate`:** Probes the keyfile against each disk. If it authenticates, reports "already enrolled" and skips that disk for the rest of enrollment. A rejected probe means the disk still needs enrollment; any other probe failure (e.g. device busy) aborts immediately rather than treating the disk as un-enrolled.
7. For each disk still needing enrollment, checks LUKS slot 1: proceeds if free; refuses with an error if occupied by an unknown key (you must remove it first with `cryptsetup luksKillSlot`).
```

Renumber the subsequent steps so the section flows 1-10:

- old `7. **With `--generate`:** ...` -> new `8.`
- old `8. Enrolls the keyfile into LUKS slot 1 on each disk.` -> new `9.`
- old `9. Creates a LUKS header backup for each modified disk.` -> new `10.`

Notes on the wording choices:

- "Probes the keyfile" + "authenticates" is the same level of user-facing
  vocabulary the existing line 63 ("before any keyfile probe") uses; no
  new jargon.
- The "aborts immediately rather than treating the disk as un-enrolled"
  clause is the operator-debugging hook the finding wanted -- it makes
  the regression behavior visible without naming exit codes.
- The `--generate` callout in the new step 6 fixes the existing manual's
  silent bug (current step 6 bullet 1 is wrong in `--generate` mode).
- Slot-1 wording collapses to a single sentence (free vs. occupied)
  because the prose form is shorter than the three-bullet form once the
  keyfile-probe bullet is hoisted out.

## Critical files

- `manual/commands/enroll.md` -- the only file to edit.

## Verification

- Read the updated `manual/commands/enroll.md` end-to-end and confirm
  the numbered steps flow 1-10 with no gaps and no duplicated numbers.
- Cross-check the new step 6 against the implementation's two-mode
  dispatch:
    - `cli/src/enroll_key_file.rs:199-220` (ExistingKeyfile probes first,
      `Authenticated` -> AlreadyEnrolled, `Rejected` falls through).
    - `cli/src/enroll_key_file.rs:222-227` (slot-1 check runs for
      disks not already classified as already enrolled -- i.e. after a
      Rejected probe in ExistingKeyfile mode, or directly in
      GenerateNew mode; `plan_single_disk_existing_keyfile_already_enrolled`
      at `cli/src/enroll_key_file.rs:2407` seeds no slot-check mock and
      pins the skip).
- Cross-check the new "aborts immediately" clause against
  `plan_keyfile_verify_busy_surfaces_open_failed_not_proceeds`
  (`cli/src/enroll_key_file.rs:2500-2548`) -- the test asserts
  `OpenFailed { exit_code: 5 }`, not a fall-through to slot-1 check.
- No code or test changes, so `just test-rust` / `just test-vm` are not
  required for this plan. (If desired as a smoke check that the docs
  edit didn't accidentally touch anything else: `git diff --stat` should
  show exactly one file changed.)
