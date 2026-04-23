# Fix: verify passphrase on PresentLuks new-disk path before journal write

## Context

In `braid replace`, when the new disk is already LUKS-formatted but closed
(`ConfigDiskState::PresentLuks { mapper_open: false }`), the code currently
writes the journal (`pending-op.json`) and acquires the logind sleep inhibitor
*before* verifying that the supplied passphrase opens the target LUKS
container. The actual verification happens via `ensure_luks_open` inside the
"irreversible" section (`cli/src/replace.rs:280`), which returns
`LuksError::OpenFailed` on exit-2 (wrong passphrase).

Result: a wrong-passphrase -- a pure reversible precondition failure with
zero disk mutation -- strands `pending-op.json` on disk and forces the
operator into `braid recover`. This contradicts decision 019
(`docs/decisions/019-inhibit-sleep.md`) and the recorded rule that
environment/preflight resource acquisition must happen **before**
`journal::write_journal`, not after (see also
`.claude/memory/feedback_acquire_env_before_journal.md`).

The `PresentNotLuks` path already does the right thing: it verifies the
passphrase against an existing pool member at lines 196-212, before the
inhibitor + journal. This plan extends the same pre-journal discipline to
the `PresentLuks { mapper_open: false }` case.

The `PresentLuks { mapper_open: true }` sub-case does not need a new check
because the replace code path does not attempt to open the mapper or
otherwise exercise the passphrase -- it relies on the probe layer's
`cryptsetup status` + backing-UUID cross-check as the source of truth for
an already-open mapper.

## Existing coverage (audit)

- `tests/cli/replace-passphrase-mismatch.py` -- covers only the
  `PresentNotLuks` (fresh disk) path.
- `tests/cli/replace-new-already-luks.py` -- covers only the
  `PresentLuks { mapper_open: false }` **success** path (correct
  passphrase).
- No existing test covers the exact bug path: wrong passphrase against a
  preformatted, closed replacement disk. No `tests/module/replace*`
  coverage exists (the `tests/module/` tree is separate -- NixOS module
  configs, not test scripts).

## Fix

In `cli/src/replace.rs`, insert a pre-journal passphrase verification for
the `PresentLuks { mapper_open: false }` case, mirroring the existing
`PresentNotLuks` block.

### Change location

`cli/src/replace.rs`, immediately after the existing reversible-check block
(line 212, after the `PresentNotLuks` verification) and before
`check_new_not_in_pool` on line 215.

### Code

Add a new block:

```rust
if let ConfigDiskState::PresentLuks { mapper_open: false, .. } = new_probed.state {
    match verify_passphrase(runner, &new_by_id.0, &passphrase)? {
        VerifyOutcome::Authenticated => {}
        VerifyOutcome::Rejected => {
            return Err(ReplaceError::Validation(format!(
                "passphrase rejected by new disk '{new_name}' ({new_by_id})"
            )));
        }
    }
}
```

Reuses `verify_passphrase` + `VerifyOutcome` already imported at the top
of the file (`cli/src/replace.rs:6-9`) and already applied for the
`PresentNotLuks` case at `cli/src/replace.rs:203-210`.

Non-EPERM failures (OOM, EINVAL, busy, etc.) still surface as
`LuksError::OpenFailed` via `?`, but because this now sits before the
journal write they also fail cleanly without stranding `pending-op.json`.

### No change needed

- `cli/src/replace.rs:278-287` (the post-journal `PresentLuks` arm) stays
  as is. `ensure_luks_open` there is now guaranteed a valid passphrase
  modulo an extremely narrow TOCTOU window.
- The `PresentNotLuks` block (lines 196-212) already verifies the
  passphrase against an existing pool member; no overlap.

## Critical files

- `cli/src/replace.rs` -- the fix location (new block near line 212) and
  the home of the two new unit tests.
- `cli/src/luks.rs` -- source of `verify_passphrase`, `VerifyOutcome`,
  `classify_verify_exit`. No change.
- `docs/decisions/019-inhibit-sleep.md` -- invariant being honored. No
  change.
- `tests/cli/replace-preformatted-luks-passphrase-mismatch.py` -- NEW VM
  test script.
- `tests/cli/replace-preformatted-luks-passphrase-mismatch.nix` -- NEW Nix
  harness (disks, packages, `testScript = builtins.readFile ./....py`).
  Every CLI VM test requires both files -- see
  `tests/cli/replace-new-already-luks.nix` as the canonical template for
  this scenario (four `emptyDiskImages`, `braid` + `cryptsetup` +
  `btrfs-progs` in `environment.systemPackages`, braid config at
  `/etc/braid/config.json`).
- `flake.nix` -- add an explicit `replace-preformatted-luks-passphrase-mismatch`
  check next to the existing `replace-new-already-luks` and
  `replace-passphrase-mismatch` entries around `flake.nix:271-280`.

## Regression tests

### 1. Unit test -- the bug fix (closed LUKS, wrong passphrase)

Add in `cli/src/replace.rs` under `#[cfg(test)] mod tests`. Pattern mirrors
`journal_survives_replace_failure` (`cli/src/replace.rs:1482`) and
`dry_run_does_not_acquire_inhibitor` (`cli/src/replace.rs:1567`). Reuse
`ReplaceMockFs`, `RecordingInhibitor`, and `StatePaths::custom`.

```rust
#[test]
// Intent: wrong passphrase on a PresentLuks { mapper_open: false } new disk
//   must fail before the journal is written.
//
// Why it exists: the closed-LUKS replacement path previously deferred
//   passphrase verification to the post-journal ensure_luks_open call, so a
//   wrong passphrase stranded pending-op.json and forced the user into
//   braid recover for a pure preflight failure (contradicts decision 019).
//   Re-introducing that ordering must flip this assertion.
//
// Scenario: operator runs `braid replace --old disk2 --new disk3=...`
//   where disk3 is already LUKS-formatted (closed mapper) and types the
//   wrong passphrase. The command must abort cleanly: no journal, no
//   inhibitor acquired, Err(Validation).
fn wrong_passphrase_on_closed_luks_new_disk_does_not_write_journal() { ... }
```

Runner shape: a small variant of `FailingReplaceRunner` that reports the
new disk as `PresentLuks { mapper_open: false }` (stub `CryptsetupLuksUuid`,
`CryptsetupLuksDumpText`, `CryptsetupStatus` for `braid-disk3` as inactive)
and returns `exit_status: 2` for the new disk's `CryptsetupTestPassphrase`.

Asserts:
- `matches!(result, Err(ReplaceError::Validation(_)))`
- `journal::load_journal(&paths).unwrap().is_none()`
- `inhibitor.acquire_count() == 0`

Fails before the fix (journal is written and inhibitor acquired before
`ensure_luks_open` reports failure), passes after. Behavioral, not
structure-sensitive.

### 2. Unit test -- mapper_open: true stays clean

Also in `cli/src/replace.rs` under `#[cfg(test)] mod tests`.

```rust
#[test]
// Intent: when the new disk's mapper is already open
//   (PresentLuks { mapper_open: true }), cmd_replace must not call
//   CryptsetupTestPassphrase or CryptsetupLuksOpen against that disk.
//
// Why it exists: the pre-journal passphrase check only targets the
//   mapper_open: false branch. Any future refactor that accidentally
//   broadens it to all PresentLuks or re-adds a post-journal
//   ensure_luks_open on the already-open path would add an unnecessary
//   credential demand. This test pins the no-op shape of the open-mapper
//   branch.
//
// Scenario: a previous replace/add already opened the mapper but never
//   added it to the pool (e.g. crash). Operator retries `braid replace`;
//   the command picks up the already-open mapper and proceeds to
//   btrfs replace start without a second LUKS interaction.
fn mapper_open_true_does_not_verify_or_open_new_disk_luks() { ... }
```

Runner shape: a small `RecordingRunner` -- same pattern as
`cli/src/remove.rs:527-548` -- that wraps the `FailingReplaceRunner` body
but appends every `CmdRequest` to an `Arc<Mutex<Vec<CmdRequest>>>` log
before dispatching. Use `MockRunner::with_mapper_open` shape for the
`braid-disk3` probe (active mapper, backing `/dev/vdd`, matching LUKS
UUID) so `probe_config_disk` returns
`PresentLuks { mapper_open: true }`. Stub `BtrfsReplaceStart` to fail
cleanly so the replace has a deterministic downstream exit point.

Asserts (direct, positive assertions on the recorded log):

```rust
let log = log.lock().unwrap();
let new_by_id = "/dev/disk/by-id/virtio-disk3";

let test_passphrase_calls = log.iter().filter(|r| matches!(
    r,
    CmdRequest::CryptsetupTestPassphrase { device } if device == new_by_id
)).count();
assert_eq!(test_passphrase_calls, 0,
    "mapper_open: true must not trigger CryptsetupTestPassphrase on the new disk");

let open_calls = log.iter().filter(|r| matches!(
    r,
    CmdRequest::CryptsetupLuksOpen { device, .. } if device == new_by_id
)).count();
assert_eq!(open_calls, 0,
    "mapper_open: true must not trigger CryptsetupLuksOpen on the new disk");
```

Plus the usual post-journal invariants to confirm the flow actually reached
the btrfs phase (proving the zero counts mean "not called," not "test
aborted early"):

- `journal::load_journal(&paths).unwrap().is_some()`
- `inhibitor.acquire_count() == 1`
- `matches!(result, Err(ReplaceError::Pool(_)))` -- i.e. the expected
  `BtrfsReplaceStart` downstream failure, not a `MissingMock`.

Direct-count assertions are insensitive to error-plumbing refactors: a
future change to `CmdError`/mock matching cannot accidentally satisfy the
test, because "zero recorded calls" is a positive fact, not the absence of
a specific error shape.

### 3. VM regression -- end-to-end bug coverage

Add `tests/cli/replace-preformatted-luks-passphrase-mismatch.py`, modeled
on the setup in `tests/cli/replace-new-already-luks.py` and the
wrong-passphrase assertions in `tests/cli/replace-passphrase-mismatch.py`.
Register it wherever the other `replace-*.py` checks are registered (check
`tests/all-tests.nix` or the equivalent aggregator).

Shape:

```python
# Phase 0: build 2-drive pool with shared `passphrase` (as in
#          replace-passphrase-mismatch.py).
# Phase 1: preformat disk3 as LUKS with the correct `passphrase`
#          (mirrors replace-new-already-luks.py phase 1 -- operator-run
#          cryptsetup luksFormat via printf '%s' | --key-file=-).
#          Assert disk3 is LUKS and /dev/mapper/braid-disk3 does NOT exist.
#          Capture luksUUID before.
# Phase 2: run `braid replace --old disk2 --new disk3=... --passphrase-stdin
#          --yes` with wrong_passphrase. Expect non-zero exit with a
#          passphrase-related stderr.
# Phase 3: assert the bug-fix invariants:
#   - /var/lib/braid/pending-op.json does NOT exist  <-- KEY ASSERTION
#   - cryptsetup luksUUID disk3 unchanged (not re-formatted)
#   - /dev/mapper/braid-disk3 still closed (not erroneously opened)
#   - Pool unchanged: disk1 + disk2 present, disk3 not a member, no missing
#   - Data (/mnt/storage/precious.txt) intact
#   - Pool membership (/var/lib/braid/pool.json) unchanged
```

Before the fix, Phase 3's pending-op.json assertion fails because
`ensure_luks_open` runs after `journal::write_journal` and strands the
journal. After the fix, verification fails before the journal is
written and pending-op.json is absent.

## Verification

1. `just test-rust` -- both new unit tests pass; existing tests (notably
   `journal_survives_replace_failure`, `dry_run_does_not_acquire_inhibitor`,
   and fresh-disk passphrase verification paths) stay green.
2. `just test-vm replace-preformatted-luks-passphrase-mismatch` -- new VM
   regression passes. For confidence, also run the two existing related
   tests (`replace-new-already-luks`, `replace-passphrase-mismatch`) to
   confirm no collateral break:
   `just test-vm replace-new-already-luks replace-passphrase-mismatch replace-preformatted-luks-passphrase-mismatch`.
3. `cargo check -p braid-cli` (or whatever the crate is) to confirm the
   diff compiles.
