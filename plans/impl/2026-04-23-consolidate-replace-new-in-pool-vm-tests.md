# Consolidate `replace --new already in pool` VM coverage

## Context

Two VM tests exist for the same scenario: `braid replace --old X --new Y`
where Y is already a pool member.

- `tests/cli/replace-new-in-pool-guard.py` asserts the precise braid-layer
  error (`"already a member"` on lines 48-54). Header is current.
- `tests/cli/replace-new-already-in-pool.py` asserts non-zero exit plus
  that the pool, data, and `pool.json` are unchanged. Header is stale --
  claims "no explicit braid-level pre-check exists... the failure currently
  comes from the btrfs layer" (lines 11-14), which is no longer true since
  `check_new_not_in_pool` exists.

The two tests together cover the full contract (rejection + no side
effects), but neither one alone does, and the stale header in the older
one is actively misleading. Merging them into the current-header test and
deleting the stale one leaves a single authoritative regression for this
guard, with stronger coverage than either test has today (the surviving
test currently does not verify `pool.json` or `pending-op.json` state).

The originally-requested "inline `check_new_not_in_pool`" is explicitly
**not** part of this plan. Keeping the named helper + its two unit tests
(`cli/src/replace.rs:1338`, `1348`) is strictly better: it names the
invariant, gives fast guard-specific feedback, and makes the merged VM
test's purpose easier to read.

## Change

### 1. Expand `tests/cli/replace-new-in-pool-guard.py`

Absorb every assertion the deleted test makes, plus one new one
(`pending-op.json` absence). After the edit, the test must assert the full
contract: **rejection + no observable side effects**.

Concretely, modify `tests/cli/replace-new-in-pool-guard.py`:

- **Header:** rewrite the entire top-of-file comment block (all three
  conventional sections: `Intent`, `Why it exists`, `Scenario`) so it
  describes the merged contract -- not just the rejection+message. Per
  `AGENTS.md` > Test Conventions and `feedback_audit_narrative_after_dropping_assumption`,
  every section must stay consistent with the new assertions:
  - `Intent`: braid-layer rejection with `"already a member"` AND no
    observable side effects (pool unchanged, data intact, `pool.json`
    bit-identical, no stranded `pending-op.json`).
  - `Why it exists`: the live `btrfs replace start` path has no natural
    duplicate guard; without `check_new_not_in_pool` the command would
    reach btrfs. Additionally, a preflight failure must not strand
    recovery state or mutate on-disk membership metadata.
  - `Scenario`: operator typo -- specifies an existing pool member as
    `--new`.
- **Before the replace attempt** (after the existing setup at lines 44-46):
  - Seed a canary file: `machine.succeed("echo 'important data' > /mnt/storage/precious.txt && sync")`
  - Capture baseline `pool.json` for later comparison (via
    `machine.succeed("cat /var/lib/braid/pool.json")`).
- **Reuse and extend the existing rejection subtest** (lines 48-54):
  keep the exit-code and `"already a member"` assertions.
- **Add new subtests after the rejection**, mirroring
  `replace-new-already-in-pool.py:76-92`:
  - `"Pool unchanged after failed replace"`: re-run `btrfs fi show
    /mnt/storage`, assert both `/dev/mapper/braid-disk1` and
    `/dev/mapper/braid-disk2` are present, assert `"missing"` does not
    appear, assert `devid` count == 2.
  - `"Data intact after failed replace"`: read `/mnt/storage/precious.txt`
    back and assert its content matches what was written.
  - `"Pool membership unchanged after failed replace"`: re-read
    `/var/lib/braid/pool.json` and compare the full parsed JSON to the
    baseline captured before the attempt -- equality of the entire
    structure, not just membership keys. `pool.json` members carry
    `by_id`, `luks_uuid`, `devid`, `added_at`, etc. (see
    `cli/src/membership.rs:30`), and a regression could mutate that
    metadata while preserving the same disk names. Use
    `assert baseline == after, ...` on parsed dicts.
  - **New** `"No journal stranded after failed replace"`:
    `machine.fail("test -e /var/lib/braid/pending-op.json")`. Direct
    regression gate for "no stranded journal on preflight failure". This
    does **not** prove the guard still runs before
    `sleep_inhibitor.acquire()` at `cli/src/replace.rs:257` -- a
    hypothetical move of the guard to after-inhibitor-but-before-journal
    would still pass this check. `pending-op.json` path confirmed via
    `cli/src/state_paths.rs:23`.
- Add `import json` at the top for `pool.json` parsing.

The 2-drive setup stays (the guard fires identically at any pool size).
The `replace_cmd` helper stays as-is.

### 2. Delete `tests/cli/replace-new-already-in-pool.py` and `.nix`

After the merge, these files have no unique coverage left.

### 3. Remove the flake registration

Edit `flake.nix:291-294` -- delete the `replace-new-already-in-pool`
entry in the `checks` set (keep the nearby `replace-new-in-pool-guard`
entry untouched).

### 4. Leave source code alone

`cli/src/replace.rs` unchanged. `check_new_not_in_pool` and both its unit
tests stay.

### 5. Leave historical plans alone

`plans/impl/2026-04-07-replace-inhibit-suspend.md:203` references the
deleted test name in a historical context -- do not rewrite history.

## Critical files

- `tests/cli/replace-new-in-pool-guard.py` -- expand assertions.
- `tests/cli/replace-new-already-in-pool.py` -- delete.
- `tests/cli/replace-new-already-in-pool.nix` -- delete.
- `flake.nix` -- remove the `replace-new-already-in-pool` check entry.

## Verification

Per `feedback_split_test_tree_mutations_from_docs.md`, test-tree mutations
require real VM runs, not greps or `nix flake check` alone.

1. `just test-vm replace-new-in-pool-guard` -- the consolidated test must
   pass. This is the load-bearing regression gate.
2. **Coverage regression check** (per plan-review protocol, "does a test
   fail when the property regresses?"): before running step 1 against the
   final code, temporarily comment out `check_new_not_in_pool(...)` at
   `cli/src/replace.rs:242` and rerun `just test-vm
   replace-new-in-pool-guard`. The test must fail -- either on the
   `"already a member"` assertion or on one of the no-side-effects
   assertions. Restore the guard and confirm the test passes again.
3. `just test-vm replace-live-disk` -- confirms the happy path (replace
   succeeds when --new is NOT already in the pool) still passes, proving
   the merged test did not inadvertently over-constrain the guard.
4. `cargo test -p braid-cli replace` -- confirms
   `new_disk_already_in_pool_rejected` and `new_disk_not_in_pool_passes`
   still pass.
5. `grep -rn --exclude-dir=plans/wip 'replace-new-already-in-pool' .` --
   should return only the single historical hit in
   `plans/impl/2026-04-07-replace-inhibit-suspend.md`. The `plans/wip`
   exclusion keeps this active plan file out of the result; that file
   will be moved or deleted when the work ships.
