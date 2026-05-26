# Pin apply_enrollment early-abort: backup failure leaves later disks unmutated

## Context

`apply_enrollment` (`cli/src/enroll_key_file.rs:285-334`) enrolls + backs up
pool disks one at a time, propagating the first error with `?`. If disk1's
keyfile enrolls but its post-mutation header backup fails
(`luks::backup_luks_header_post_mutation`, `cli/src/luks.rs:542`), the loop
returns at line 316 before disk2 is ever touched.

That early-abort ordering is a load-bearing recovery contract, not an
incidental implementation detail. Decision-019
(`docs/design/decisions/019-inhibit-sleep.md:187-192`) justifies *not*
acquiring a sleep inhibitor for standalone `braid enroll` specifically because
"a partial enroll leaves only the un-enrolled disks for the next invocation":
re-running enroll re-detects already-enrolled disks as `AlreadyEnrolled` and
finishes the rest. If a refactor enrolled all disks before backing any up (or
swallowed the backup error and continued), disk2 would be mutated before the
abort and would come back `AlreadyEnrolled` on the recovery re-run -- with its
own header backup silently skipped -- quietly breaking that contract.

No existing test pins this:
- `apply_enrollment_returns_enriched_error_when_backup_fails`
  (`cli/src/enroll_key_file.rs:3056`) uses a **single-disk** plan, so it cannot
  observe a second disk being left alone.
- `cmd_enroll_apply_failure_does_not_write_pending_op_journal`
  (`cli/src/enroll_key_file.rs:3363`) is two-disk but fails at disk2's
  **enroll** while disk1's backup **succeeds**; it asserts journal absence, not
  disk2-unmutated on a disk1 **backup** failure.

This plan adds one regression test. It is **test-only**: the production
behavior is already correct. The "surface a clear 'later disks not enrolled'
message" idea from the original finding is deliberately out of scope -- the
enriched error plus the documented idempotent re-run is the recovery contract,
and `apply_enrollment` already prints a per-disk `[ok] ... keyfile enrolled`
line so a missing disk's absence is already visible.

## The fix

Add one `#[test]` in the `enroll_key_file.rs` test module, placed immediately
after `apply_enrollment_returns_enriched_error_when_backup_fails`
(after `cli/src/enroll_key_file.rs:3100`), so the two sibling tests sit
together. Suggested name: `apply_enrollment_backup_failure_leaves_later_disk_unmutated`.

It calls `apply_enrollment` directly (not through `cmd_enroll_key_file`), the
same entry point the single-disk template test uses, with a **two-disk**
`NeedsEnroll` plan.

### Reused helpers (all already used by the template test)

- `isolated_paths() -> (TempDir, StatePaths)`
- `enroll_add_keyfile_ok(device, key_file, passphrase) -> (CmdRequest, Vec<u8>, RawCommandOutput)`
  (builds `CmdRequest::CryptsetupLuksAddKeyFile`)
- `enroll_err_raw(label, exit, stderr) -> RawCommandOutput`
- `enroll_by_id`, `enroll_passphrase`, `disk` (the `fn disk` in this module)
- `MockRunner::{with_output_stdin, with_output}` and `runner.requests()`

### Runner setup

Register, via the chained `MockRunner` builder:
1. disk1 enroll-ok -- `enroll_add_keyfile_ok(d1, kf, pass)` -> `with_output_stdin(...)`.
2. disk1 header-backup **fail** -- the exact `CmdRequest::CryptsetupLuksHeaderBackup`
   the template test registers (device `d1`, backup_path
   `paths.luks_headers_dir().join("braid-disk1.luksheader.tmp")`), paired with
   `enroll_err_raw("cryptsetup luksHeaderBackup", 1, "No space left on device")`.
3. disk2 enroll-ok -- `enroll_add_keyfile_ok(d2, kf, pass)` -> `with_output_stdin(...)`.

Step 3 is the deliberate design choice: `MockRunner` is lenient (an
unconsumed registered output is fine -- no exhaustion check; `cmd.rs` has no
`Drop`/`verify`), so registering disk2's enroll-ok costs nothing on the correct
path (the loop aborts at step 2 and never consumes it). Its payoff is on a
regression: a "pre-enroll all disks, then back up" reorder would *consume*
step 3 and record disk2's `CryptsetupLuksAddKeyFile`, so the absence assertion
below fails with a clear message instead of a muddy `CmdError::MissingMock`.
Do **not** register a disk2 header-backup -- if a regression ever reaches it,
disk2's enroll request is already recorded (requests are logged before dispatch,
`cmd.rs:1575`/`1589`), so the assertion still bites.

### Plan

```rust
let plan = vec![
    DiskEnrollAction::NeedsEnroll { name: disk("disk1"), by_id: enroll_by_id(d1) },
    DiskEnrollAction::NeedsEnroll { name: disk("disk2"), by_id: enroll_by_id(d2) },
];
```

`name: disk("disk1")` is required so the mapper resolves to `braid-disk1` and
the backup path matches the registered failing-backup mock.

### Assertions

Call `apply_enrollment(&runner, &plan, &enroll_passphrase(pass), Path::new(kf), &paths)`,
`.expect_err(...)`, then inspect `runner.requests()` (the function borrows the
runner, so it stays usable):

1. **Error is the enriched backup failure** -- same three string checks as the
   template test: contains `"cryptsetup luksHeaderBackup --header-backup-file"`,
   contains `d1`, contains `"after the LUKS mutation completed"`.
2. **disk1 enrolled *before* its header backup, and reached the failing
   backup** (pins the post-mutation ordering, not just presence): locate both
   indices in `requests()` with the `.position(|r| matches!(...))` idiom (cf.
   `recover.rs:7935`, `:9717`) -- `add_pos` for
   `CryptsetupLuksAddKeyFile { device, .. } if device == d1` and `backup_pos`
   for `CryptsetupLuksHeaderBackup { device, .. } if device == d1`, each
   `.expect(...)`-ed present -- then assert `add_pos < backup_pos` (message:
   post-mutation backup ordering, enroll must precede backup). Structure-
   insensitive: it pins enroll-before-backup without asserting exact indices.
3. **disk2 left untouched** (the load-bearing property): `requests()` contains
   **no** `CryptsetupLuksAddKeyFile` for `d2` **and no** `CryptsetupLuksHeaderBackup`
   for `d2`, using the negated `.any(... matches! ... if device == d2)` idiom
   (cf. `recover.rs:6792`, `mount.rs:2842`). Give each a message tying it to the
   recovery contract, e.g. `"disk2 must be left for the idempotent re-run"`.

### Test preamble

Follow the repo's three-section convention (Intent / Why it exists / Scenario;
see `docs/dev/testing.md`). Intent: the loop aborts at the first disk's
post-mutation backup failure and leaves every later disk fully unmutated. Why
it exists: decision-019's recoverability justification for skipping the sleep
inhibitor is ordering-dependent; the single-disk and journal-absence tests
don't cover disk2-unmutated on a disk1 *backup* failure, so a reorder/swallow
refactor would pass them. Scenario: operator runs `braid enroll DIR` over a
two-disk pool, disk1 enrolls but its local `.luksheader` backup fails (state
dir out of space); enroll aborts with the manual-backup remediation for disk1
and disk2 is untouched for the re-run.

## Out of scope

- No change to `apply_enrollment` or any production code.
- No new user-facing "disk2 not enrolled" stderr line (see Context).
- No doc change -- decision-019 already states the contract this test pins.

## Verification

- `just test-rust` runs the full Rust unit-test set (fixed recipe -- it takes
  no arguments). For a focused run during iteration use
  `cargo test --lib apply_enrollment_backup_failure_leaves_later_disk_unmutated`
  (this test lives in the `--lib` lane, matching `test-rust`'s own
  `cargo test --lib ...`). Expect it to **pass** against current code
  (production behavior is already correct).
- Optional robustness check (do not commit): temporarily reorder
  `apply_enrollment` to enroll both disks before backing up, confirm the new
  test fails on the "disk2 must be left for the idempotent re-run"
  `CryptsetupLuksAddKeyFile` assertion, then revert. This proves the test bites
  the regression it exists to catch.
- No VM tests needed -- this is a pure Rust unit test with no module/systemd
  surface.
