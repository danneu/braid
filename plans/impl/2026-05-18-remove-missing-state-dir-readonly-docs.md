# Plan: Fix stale `remove-missing-membership-readonly` test docs

## Context

The NixOS VM test `tests/cli/remove-missing-membership-readonly.py` carries
a stale preamble. Its `Why it exists` section claims the test pins
`remove_missing.rs:158-161` — a "warn on `save_membership` failure and
proceed with btrfs device deletion" regression. That claim was accurate
at test birth (commit `a9b7467`, when `save_membership` really was the
*first* `/var/lib/braid` write in remove-missing, used a
`if let Err(e) = ... { eprintln!("warning: ..."); }` warn-and-proceed
pattern, and there was no journal).

Today the code follows the ADR-017 (`docs/decisions/017-runtime-disk-membership.md`,
"Mutation ordering") sequence: write `pending-op.json` (journal) ->
perform the irreversible btrfs membership change -> write `pool.json` to
reflect committed live membership -> advance the journal to a
post-maintenance phase -> post-mutation maintenance -> clear journal.

Concretely in `cli/src/remove_missing.rs`:

- `:243-244` writes the journal (`pending-op.json` under `/var/lib/braid`)
  *before* `pool_remove_device_using` runs.
- `:256-263` performs `pool_remove_device_using` (the irreversible
  btrfs membership change).
- `:276-278` calls `save_membership` (writes `pool.json`) *after* the
  btrfs remove, and propagates errors via `?` (fail-hard, not
  warn-and-proceed).

Because `pending-op.json` and `pool.json` both live under
`/var/lib/braid` (`cli/src/state_paths.rs:23, 19`), the test's read-only
bind mount fails `journal::write_journal` first -- before any btrfs
mutation. That is the *only* failure phase this VM setup can actually
exercise: a later `save_membership` failure would only fire after
`btrfs device remove` has already committed, and the recovery model for
that phase is journal-protected (the journal survives; `braid recover`
reconciles per ADR-017 line 47), not "pool untouched".

The test's outer assertions (`exit != 0`, `"missing" in fi_show`, data
intact) still pass, but the file name, the `# Test:`/`What`/`Why`
blocks in both the `.py` and the `.nix` sibling, the in-body subtest
labels and `.pool.json.tmp` comment, and the cited code location all
overpromise: they read as if the test pins the whole class of
"state-dir write failure -> pool untouched", when in fact it only pins
the journal-write-first-failure phase.

The ideal fix is to align names, preambles, and in-body comments with
the narrower, accurate invariant: when the pending-operation journal
cannot be written, remove-missing aborts before any btrfs mutation. The
read-only bind-mount setup stays -- it is the canonical way to exercise
atomic-write failure under root in a VM, and it is exactly the journal
write that it blocks.

## Changes

### 1. Rename test files

- Rename `tests/cli/remove-missing-membership-readonly.py` →
  `tests/cli/remove-missing-state-dir-readonly.py`
- Rename `tests/cli/remove-missing-membership-readonly.nix` →
  `tests/cli/remove-missing-state-dir-readonly.nix`

Rationale: the bind mount makes the whole state directory read-only, not
just the membership file. The new name describes the canonical scenario
(state-dir read-only) without overpromising a `save_membership`-specific
pin that the VM setup cannot deliver.

### 2. Update `flake.nix:547-548`

Replace the test key and import path:

```nix
remove-missing-state-dir-readonly = pkgs.testers.nixosTest (
  import ./tests/cli/remove-missing-state-dir-readonly.nix {
    braid = linuxCrane.braid;
  }
);
```

### 3. Rewrite `.nix` sibling header, `What`/`Why`, `name`, and `readFile`

In the renamed `tests/cli/remove-missing-state-dir-readonly.nix`:

- Lines 1-11 (header + `What`/`Why`/`Dependencies` block) -> replace with
  the following. This carries the same rescoped framing as the .py
  preamble but in the .nix sidecar's terser What/Why form:

  ```nix
  # Test: remove-missing-state-dir-readonly
  #
  # What: `braid remove-missing` must fail hard when the pending-operation
  # journal cannot be written. The btrfs pool must stay intact.
  #
  # Why: Per ADR-017 ("Mutation ordering"), pending-op.json is written
  # before the irreversible btrfs membership change. If that write fails
  # (read-only state dir, ENOSPC, permissions), remove-missing must abort
  # before any btrfs mutation -- otherwise btrfs and pool.json could
  # diverge with no journal to drive recovery.
  #
  # Dependencies: braid add (builds the test pool).
  ```

- Line 14 (`name = "remove-missing-membership-readonly";`) -> update to
  `name = "remove-missing-state-dir-readonly";`.
- Line 45 (`testScript = builtins.readFile
  ./remove-missing-membership-readonly.py;`) -> update path to
  `./remove-missing-state-dir-readonly.py`.

### 4. Rewrite the `.py` preamble (lines 1-17)

Per repo `Test Conventions` in `AGENTS.md` and `docs/testing.md` (Intent
/ Why it exists / Scenario), replace the existing preamble with the
following content (form preserved -- `#` comments, three labeled
sections, top `# Test:` headline). Use plain ASCII per global style
(`--`, not em-dash):

```python
# Test: braid remove-missing aborts when the state directory is read-only
#
# Intent:
#   `braid remove-missing` must fail hard (exit non-zero) when the
#   pending-operation journal cannot be written to /var/lib/braid. The
#   btrfs pool must stay intact: the journal write is the first
#   /var/lib/braid write in remove-missing, so a fully read-only state
#   dir aborts the command before pool_remove_device_using runs.
#
# Why it exists:
#   Per ADR-017 (docs/decisions/017-runtime-disk-membership.md,
#   "Mutation ordering"), every mutating command writes pending-op.json
#   BEFORE the irreversible btrfs membership change, then writes
#   pool.json AFTER btrfs commits. This test pins the
#   journal-write-fails-first half of that invariant: if
#   journal::write_journal cannot persist pending-op.json (read-only
#   filesystem, ENOSPC, permissions), no btrfs mutation is permitted.
#
#   Scope note: this test does not -- and structurally cannot -- pin
#   the post-mutation pool.json write phase. When the test was added
#   (commit a9b7467), save_membership was the FIRST write in
#   remove-missing and only logged a warning on failure -- the read-only
#   bind mount caught exactly that. Today journal::write_journal
#   (cli/src/remove_missing.rs ~line 243) precedes the btrfs mutation,
#   and save_membership (~line 276) sits after it and propagates errors
#   via `?`. A post-btrfs save_membership failure is a different
#   failure class: btrfs has committed, the journal survives, and
#   `braid recover` is responsible for reconciliation per ADR-017
#   ("Mutation ordering" / recovery model). save_membership's position
#   around btrfs device remove is covered at the unit-test seam by
#   `journal_survives_soft_balance_failure` in
#   cli/src/remove_missing.rs, not here.
#
# Scenario:
#   /var/lib/braid becomes read-only (disk full, permissions issue, or
#   filesystem error) while the operator runs `braid remove-missing`.
#   The journal write fails first, so the command refuses to mutate
#   btrfs.
```

### 5. Refresh in-body comments and subtest labels

The Phase 2 block (test body lines 60-67 and 81) still frames the
scenario in `save_membership`/`pool.json` terms, which the renamed,
rescoped preamble contradicts. Update these strings only -- commands,
flow, and assertions stay identical:

- Line 60 (`# --- Phase 2: Make membership dir read-only, then attempt
  remove-missing ---`) -> rewrite to `# --- Phase 2: Make state
  directory read-only, then attempt remove-missing ---`.
- Line 62 (`with subtest("Make membership dir read-only"):`) -> rewrite
  to `with subtest("Make state directory read-only"):`.
- Line 63 (`# atomic_write creates .pool.json.tmp in the same directory
  then renames.`) -> rewrite to `# atomic_write creates
  .pending-op.json.tmp in the same directory then renames -- this is the
  first /var/lib/braid write in remove-missing, so a read-only state
  dir blocks it before any btrfs mutation.`
- Line 81 (`with subtest("remove-missing with read-only membership dir
  fails"):`) -> rewrite to `with subtest("remove-missing with read-only
  state directory fails"):`.

No other body changes. The bind-mount setup
(`mount --bind` + `mount -o remount,bind,ro`), the three outer
assertions (`status != 0`, `"missing" in fi_show`, data intact), the
disk setup, and the simulated-death flow are all unchanged. Note
that the second `Pool still has missing device` subtest label
already reads cleanly without the word "membership"; leave it alone.

## Critical files

- `tests/cli/remove-missing-membership-readonly.py` -- rename + rewrite
  preamble + refresh Phase 2 in-body comment and subtest labels
  (lines 60, 62, 63, 81).
- `tests/cli/remove-missing-membership-readonly.nix` -- rename + update
  header, `What`/`Why` block, `name`, `readFile` path.
- `flake.nix:547-548` -- update test key + import path.

## Reference files (no edits)

- `cli/src/remove_missing.rs:243-244, 256-263, 276-278` — current
  execute-flow ordering the rewritten preamble describes.
- `cli/src/remove_missing.rs:1539` — the
  `journal_survives_soft_balance_failure` unit test the new preamble
  cross-references.
- `cli/src/state_paths.rs:19, 23` — confirms `pool_json()` and
  `pending_op_json()` both resolve under `/var/lib/braid`.
- `docs/testing.md`, `AGENTS.md` "Test Conventions" — preamble form.

## Out of scope

- Adding a Rust unit test that fails `save_membership` specifically via
  failure injection (would close the residual gap where a
  warn-and-proceed regression on save_membership only is invisible to
  the VM test). Tracked as a follow-up if desired; not part of this docs
  pass.
- Updating references in `plans/impl/2026-05-06-unify-cli-plan-execution.md`
  (historical, already implemented) and `plans/wip/bubbly-toasting-cerf.md`,
  `plans/wip/robust-booping-island.md` (forward-looking working docs;
  will be updated when their owners next touch them). The test rename is
  a leaf change with no behavior impact, so stale references in plan
  docs are low cost.

## Verification

1. Confirm no remaining references to the old name in the active tree:
   ```
   git grep -n "remove-missing-membership-readonly" -- ':!plans/'
   ```
   Expected: no matches outside `plans/`.

2. Run the renamed test to confirm it still passes end-to-end:
   ```
   just test-vm remove-missing-state-dir-readonly
   ```
   Expected: green. The body and bind-mount setup are unchanged, so the
   pass/fail behavior is identical to before the rename.

3. Sanity-check that the broader VM suite still discovers the test:
   ```
   nix flake show 2>/dev/null | grep remove-missing-state-dir-readonly
   ```
   Expected: present under `checks.aarch64-darwin` (or the host's
   equivalent).
