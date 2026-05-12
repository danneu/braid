# Plan: surface dangling by-id symlinks during discover

## Problem

`cli/src/discover.rs:151-156` runs `CryptsetupIsLuks` before
`canonicalize`. A dangling `/dev/disk/by-id/` symlink causes
`cryptsetup isLuks` to exit non-zero (the `crypt_init` call at
`reference/cryptsetup/src/cryptsetup.c:2475` fails when the device
path cannot be opened) and is silently dropped via the `continue`
at line 154-156. The implementation only needs the non-zero exit
behavior; the specific errno is not load-bearing.

The existing `CannotCanonicalize` warning at `discover.rs:215-224`
only fires when an entry has already passed the LUKS+label+name
filters, so observability depends on whether the dangling entry
happens to have a sibling valid alias the runner can see.

The test `discover_skips_entry_when_canonicalize_fails` (line
850-886) appears to exercise the dangling case, but it uses a
`DiscoverLabelMap` mock that returns `isLuks` success for any path
in its labels map (`test_fixtures/discover.rs:66-75`) -- so the
dangling entry artificially reaches `canonicalize`. In production,
the dangling entry never gets that far.

Operator-visible symptom: a stale by-id symlink left after a disk
swap, udev race, or hand-edit produces no diagnostic in
`braid discover`, leaving the operator confused about why a disk
"isn't seen." This is load-bearing because principle 5 (stable
identifiers) and the recovery story both depend on by-id integrity.

## Fix

Single edit to `cli/src/discover.rs`, in the
`for entry in entries.flatten()` loop of `discover_from_dir`.

Move the canonicalize block from line 215-224 to immediately after
`path_str` is built (line 148), so it runs before the
`CryptsetupIsLuks` call:

```rust
let path = entry.path();
let path_str = path.to_string_lossy().to_string();

// Catch stale udev by-id symlinks before the LUKS probe. A dangling
// symlink is a structural by-id problem independent of LUKS state
// and deserves a CannotCanonicalize warning regardless of whether
// the underlying device would have been a braid member.
let canonical = match resolver.canonicalize(&path_str) {
    Ok(c) => c,
    Err(e) => {
        warnings.push(DiscoverWarning::CannotCanonicalize {
            path: path_str.clone(),
            detail: e.to_string(),
        });
        continue;
    }
};

let raw = runner.run(&CmdRequest::CryptsetupIsLuks {
    device: path_str.clone(),
})?;
...
```

Then delete the now-redundant `let canonical = match resolver.canonicalize(...)` block at lines 215-224 -- `canonical` is already
in scope when the `members.entry(...)` match runs at line 229.

Leave the partition-entry check (line 143) at the top of the loop --
it's a pure string match with no syscalls and correctly filters
`-partN` entries before any I/O.

That is the entire functional change. New loop body order:

1. Skip partition entry.
2. Build `path_str`.
3. **Canonicalize -> warn `CannotCanonicalize` on failure.** (moved)
4. `CryptsetupIsLuks` -> silent continue on exit != 0.
5. `CryptsetupLuksDumpText` -> warn `LuksDumpFailed` /
   `LuksDumpUnparseable`.
6. Version check -> warn `UnsupportedLuksVersion`.
7. Label + braid-prefix + valid-name checks (silent or
   `InvalidDiskName`).
8. Insert into `members` with collision detection and priority
   tiebreak, reusing the already-computed `canonical`.

## Test changes

### New regression test

Add `discover_warns_on_dangling_symlink_with_no_luks_device` to the
`tests` module in `cli/src/discover.rs`:

- Create a single dangling by-id symlink via
  `discover_create_by_id_symlink(dir.path(), "ata-DANGLING_OLD",
  "/nonexistent/dangling/target")`.
- Use `DiscoverLabelMap::new(&[])`. The dangling path is NOT in the
  labels map -- this is the load-bearing distinction from the
  existing canonicalize test, which artificially mocks the dangling
  path as LUKS-positive.
- Assert:
  - `outcome.members.is_empty()`,
  - `outcome.warnings.len() == 1`,
  - the single warning matches `DiscoverWarning::CannotCanonicalize`
    with `path.ends_with("ata-DANGLING_OLD")`.

This test fails on `master` (the current code path is the silent
`continue` at line 154-156 -- the runner returns exit=1 for the
unlisted dangling path, so `warnings` stays empty) and passes after
the reorder. It pins the canonicalize gate ahead of the LUKS gate.

Preamble (Intent / Why it exists / Scenario):

- **Intent:** a dangling by-id symlink with no underlying LUKS
  device produces a single `CannotCanonicalize` warning and no
  member.
- **Why it exists:** the LUKS probe used to fail-silent on dangling
  symlinks, so operators saw no diagnostic when udev left a stale
  alias behind. Pinning canonicalize ahead of the probe makes the
  warning fire on structural by-id problems regardless of LUKS
  state.
- **Scenario:** after a disk swap, udev failed to clean up the
  prior drive's `/dev/disk/by-id/ata-OLD_DRIVE` symlink; the
  operator runs `braid discover` and expects to see why the entry
  is being skipped.

### Existing-test preamble update

`discover_skips_entry_when_canonicalize_fails` (line 850-886) is
otherwise correct after the reorder -- its dangling-plus-valid-alias
scenario is now a strict superset of the standalone-dangling case.
Update only the preamble's "Why it exists" line to reflect the new
flow: the canonicalize check runs at the by-id structural gate, not
as part of the LUKS-flow collision detection. The test body and
assertions stay as-is.

### No other test changes expected

Spot-checked the other discover tests:

- `discover_propagates_runner_error_at_isluks` -- creates a real
  symlink to a real target; canonicalize succeeds; unaffected.
- `non_luks_device_never_reaches_luks_dump`,
  `discover_warns_when_labeled_disk_fails_luksdump`,
  `discover_warns_on_unparseable_luksdump_output`,
  `discover_prefers_wwn_over_ata`,
  `discover_same_priority_breaks_ties_lexicographically`,
  `discover_skips_luks1_disk`,
  `discover_warns_on_invalid_disk_name_in_braid_label`,
  `discover_selects_best_symlink_per_disk_independently`,
  `discover_fails_on_label_collision_across_disks` -- all use real
  tempdir targets via `discover_create_target`; canonicalize
  succeeds; unaffected.
- `label_collision_sorts_paths_lexicographically` is pure unit on
  `label_collision`; unaffected.

## Risks / trade-offs

- **Extra syscall per non-LUKS entry.** Every by-id entry now does
  one `canonicalize` syscall regardless of LUKS state. Typical
  by-id counts are 10-30; cost is dwarfed by the cryptsetup
  subprocess spawn that already runs on every entry. No measurable
  impact.
- **New warnings for legitimately stale non-braid symlinks.** If a
  host has e.g. `usb-OLD_THUMBDRIVE` dangling, `braid discover`
  will emit a `CannotCanonicalize` warning for it. This is the
  intended behavior per the finding (principle 5: by-id integrity
  is operationally load-bearing) and matches what users already see
  today for stale braid-labeled aliases. Operator response is the
  same in both cases: clean up the stale udev entry, or accept the
  warning.
- **No NixOS VM test added.** Staging a real dangling by-id symlink
  in a VM is awkward because udev actively manages that directory.
  The unit-level regression against `DiscoverLabelMap` plus a real
  tempdir symlink already exercises `canonicalize` against the real
  filesystem, which is the only behavior under test.

## Out of scope

- `recover.rs:117-168` (`resolve_by_id_for_underlying`) iterates
  by-id and canonicalizes too, but it intentionally silent-skips
  dangling symlinks (explicit comment at line 142). Its job is
  target-matching for one known kernel device, not enumeration for
  pool membership -- a dangling symlink there genuinely cannot
  match anything, so silent-skip is correct. Leave alone.
- No new `DiscoverWarning` variant. `CannotCanonicalize` already
  carries the OS error message in `detail`, which is sufficient to
  distinguish ENOENT from other failure modes when an operator
  reads the warning.
- No change to `main.rs` rendering. Warnings already flow through
  the `eprintln!("warning: {warning}")` loop at
  `cli/src/main.rs:723-725`.

## Docs to update

The "What happens under the hood" section in
`manual/commands/discover.md:45-56` lists the per-entry probe order
and currently puts `cryptsetup isLuks` (step 4) ahead of any
mention of canonical resolution (step 8, as part of alias picking).
That ordering becomes wrong once canonicalize moves to the top of
the loop, and the doc has no mention of the stale-by-id warning
operators will now see.

Plan changes to `manual/commands/discover.md`:

- Insert a new bullet ahead of the current step 4 in the "What
  happens under the hood" list: *"Resolves each by-id symlink to
  its canonical kernel device. Skips with a `cannot canonicalize`
  warning when the symlink is dangling (e.g., udev didn't clean up
  after a disk removal)."*
- Renumber subsequent steps accordingly.
- Edit the existing step that mentions alias canonical resolution
  (current step 8) so it reads *"Uses the canonical kernel device
  resolved above to detect ..."* -- avoids implying canonicalize
  runs twice.
- Add a bullet to the "Safety checks" section: *"Dangling
  `/dev/disk/by-id/` symlinks are skipped with a warning -- a
  diagnostic operators need when udev leaves a stale alias behind
  after a disk swap."*

No `README.md` or `docs/principles.md` change. Principle 5 already
declares by-id integrity load-bearing; this fix is an instance of
honoring it, not a change to the principle itself.

## Implementation order

TDD per `docs/principles.md:49-51` and `AGENTS.md:197-199`: red
phase first, then green.

1. Add the new regression test
   `discover_warns_on_dangling_symlink_with_no_luks_device` to the
   `tests` module in `cli/src/discover.rs`. Run `cargo test --lib
   discover_warns_on_dangling_symlink_with_no_luks_device` (the
   `just test-rust` recipe takes no args -- positional args become
   recipe names and fail) and confirm it FAILS with
   `outcome.warnings.len() == 0` (or the equivalent matcher
   mismatch) -- this is the red phase that proves the test pins
   the dangling-silent-skip behavior.
2. Edit `cli/src/discover.rs`: move the canonicalize block to
   immediately after `path_str` is built; remove the duplicate at
   lines 215-224.
3. Re-run `just test-rust` -- the new test should now pass, and no
   pre-existing discover test should regress. Also confirm no
   `unused_variables` warning on `canonical`.
4. Update the preamble of
   `discover_skips_entry_when_canonicalize_fails` to reflect the
   new flow (canonicalize at the by-id structural gate, not as
   part of the LUKS flow).
5. Update `manual/commands/discover.md` per the "Docs to update"
   section above.
6. Final `just test-rust` and `just test-vm` (the discover VM
   tests under `tests/` should be untouched, but a full run is
   cheap insurance against indirect breakage).
