# Plan: fix stale `replace-live-disk` test preambles

## Context

`tests/cli/replace-live-disk.py` carries a header-comment preamble whose
**Intent** and **Why it exists** sections describe an obsolete design: "the new
disk is added first (RAID1 balance), then the old disk is evicted (device remove
+ LUKS close)" via a "refactored shared eviction helper." That add+balance+remove
path no longer exists. The live-replace path now does a single in-place
`btrfs replace start` (devid-preserving), then closes the old disk's LUKS mapper
-- confirmed in `cli/src/replace.rs#ReplacePlan::render_steps` (step list:
LUKS format/open -> `BtrfsReplaceStart` -> `CryptsetupClose` old mapper ->
resize), and pinned by in-tree assertions `replace.rs` ("live path should NOT
show btrfs device remove", "live path should NOT show soft balance").

The **test body already asserts the correct behavior** (`pool: replacing devid` /
`replace complete` rows, old mapper closed afterward). Only the prose is stale.
Per AGENTS.md Test Conventions, a test's Intent must match what the test proves;
a reader trusting the current preamble could "fix" a future regression in the
wrong direction, or delete this test believing `replace-preserves-devid.py`
already covers it.

The same stale framing is **duplicated in the `.nix` wrapper**
(`tests/cli/replace-live-disk.nix`: "Validates the add-first ordering ... the old
disk is fully evicted (removed + LUKS closed)" / "preserves add-first ordering").
The finding flagged only the `.py`; fixing one and leaving the other is a
half-fix that re-spawns the identical finding. The ideal fix corrects both.

Outcome: both preambles describe the actual `btrfs replace start` live path at
the level of the behavioral contract it locks (not an exhaustive stderr
inventory), and explicitly delineate this test from its sibling so neither is
mistaken for redundant.

## Scope

**In scope (2 files, comments only):**

- `tests/cli/replace-live-disk.py` -- rewrite the **Intent** and **Why it
  exists** sections (lines 5-14). Keep the **Scenario** section (lines 16-20)
  verbatim -- it is already accurate.
- `tests/cli/replace-live-disk.nix` -- rewrite the **What** and **Why** sections
  (lines 3-10). Keep **Dependencies** (line 12) and the entire Nix body
  (line 13 onward) untouched.

**Explicitly out of scope (verified already correct):**

- `tests/cli/replace-preserves-devid.py` / `.nix` -- mention add+balance+remove
  correctly, as the *negative* the test proves was not used. Use the `.nix` as
  the style template; do not touch.
- `tests/cli/replace-live-disk-busy.py` / `.nix` -- the third test that drives
  the live-replace path. Considered and excluded: its preamble already describes
  the current `btrfs replace` + best-effort EBUSY-close behavior accurately and
  carries no stale add-first/eviction framing (verified separately -- the same
  denylist pattern from Verification step 1 returns no matches against these two
  files), so it needs no change.
- `tests/cli/add-inhibits-suspend.py` -- its add/balance mention is about the
  `add` command, unrelated to the live-replace framing.
- No test-body code changes. No `flake.nix` change (the registration carries no
  description string). No behavior change anywhere.

## Change 1: `tests/cli/replace-live-disk.py` preamble

Replace lines 3-14 (the Intent and Why-it-exists sections; preserve the
surrounding structure and house style -- a section-purpose line followed by a
nested content bullet, matching the rest of the repo's `.py` preambles). The
Intent is deliberately contract-level -- the behaviors guaranteed, not a
line-by-line stderr inventory (see Claim audit for why):

```python
# Intent:
# - What behavior this test (tries to) verify.
#   - `braid replace --old <live> --new <new>` replaces a live, present disk
#     in a healthy pool in place with a single `btrfs replace start` -- the
#     `pool: replacing devid` / `replace complete` progress rows identify the
#     replace-start path, not add+balance+remove -- and closes the old disk's
#     LUKS mapper once the replace completes, leaving the pool healthy and
#     redundant with data and `pool.json` membership intact. The same command
#     also enrolls a keyfile in-step (`--enroll`), and the live path rejects
#     `--missing-id` and refuses to run once the pool has degraded, pointing
#     the operator at the correct full-syntax repair.
#
# Why it exists:
# - What risk/regression this protects against.
#   - Before this feature, `braid replace` only accepted dead/missing disks;
#     live replace is the in-place upgrade path. This test locks that path's
#     operator-visible behavior -- the progress rows, the in-step `--enroll`,
#     and the error/repair guidance -- against silent regression. It is
#     distinct from `replace-preserves-devid.py`, the narrow TDD signal that
#     `btrfs replace start` (not add+balance+remove) was used, proven via the
#     preserved devid. Neither test subsumes the other: deleting either drops
#     real coverage.
```

Lines 1-2 (the `# Test:` title + blank) and lines 15-20 (the Scenario section)
stay exactly as they are.

## Change 2: `tests/cli/replace-live-disk.nix` preamble

Replace lines 3-10 (the What and Why sections; keep the wrapper's What / Why /
Dependencies structure, matching the accurate `replace-preserves-devid.nix`):

```nix
# What: Runs `braid replace --old <live> --new <new>` to replace a live,
# present disk in a healthy pool in place with a single `btrfs replace start`,
# closing the old disk's LUKS mapper once the replace completes and leaving the
# pool healthy and redundant. Also covers the in-step `--enroll` keyfile path
# and the live-path guards that reject `--missing-id` and a degraded pool.
#
# Why: Before this feature, replacing a live disk meant orchestrating a
# separate `braid remove` + `braid add` by hand. The unified `braid replace`
# swaps the disk in place with `btrfs replace start` -- one operator step, and
# the source stays in the pool until the copy completes, so the array is never
# degraded mid-swap.
```

The `# Test: replace-live-disk` title, the `# Dependencies:` line, and the Nix
attribute set below stay unchanged.

## Claim audit (verify the new prose against the test body before editing)

Every assertion the rewrite makes is grounded in the current test/impl -- spot
check during implementation so the fix does not trade one false claim for
another:

- **`btrfs replace start`, not add+balance+remove** -- `replace.rs#ReplacePlan::render_steps`
  emits one `CmdRequest::BtrfsReplaceStart`; no `btrfs device add/remove`/balance
  on the live path (`replace.rs` negative assertions confirm).
- **`pool: replacing devid` / `replace complete` rows** -- `replace-live-disk.py`
  Phase 1 (the `repl_wait` / `repl_ok` asserts).
- **Old mapper closed after replace** -- Phase 1 (`disk disk2: locking/locked`,
  ordered after replace ok) + the `test -e /dev/mapper/braid-disk2` fail subtest.
- **RAID1 intact / 3 devids / no missing** -- the post-replace health subtest.
- **`--enroll` keyfile path** -- Phase 1b (`disk disk5: enrolling keyfile in slot 1`).
- **`--missing-id` rejected + degraded-pool rejection with repair guidance** --
  Phase 2 (both `machine.execute` guard subtests).
- **"array never degraded mid-swap"** -- property of `btrfs replace`: the source
  device stays in the pool until the copy completes, so redundancy is preserved
  throughout. Motivation claim in the Why section, not a test assertion. Do not
  resurrect the claim that the manual `remove` + `add` alternative *always* drops
  redundancy -- on a >=3-device RAID1 (including this test's 3-disk pool),
  `btrfs device remove` relocates chunks and keeps two copies; it degrades only
  in narrower cases (e.g. a 2-disk pool). The Why states only the always-true
  positive property and omits any unconditional claim about the alternative.
- **Deliberate non-claim (the anti-rot guard)** -- the Intent states the
  behavioral contract, not a totality of stderr. The live path also prints lines
  the test does not assert -- `LUKS header backed up: <path>` and the final
  `Done. Replaced <old> with <new>.` (see `replace.rs#ReplacePlan::execute`,
  verified: `rg 'header|backed up|Done\.|Replaced'` on the test returns nothing).
  The preamble must not imply those are locked. Keep the test body as the source
  of truth for the exact `[wait]/[ok]` row set, so adding or removing an
  assertion later does not re-stale the comment -- the same drift that produced
  the original stale preamble.

## Verification

Comments only -- no executable behavior changes, so the test cannot regress from
this edit. Verify the fix is complete and accurate rather than re-running the VM:

1. **No stale framing remains** (primary check):
   ```sh
   rg -n 'add-first|added first|eviction helper|device remove \+ LUKS|evicted \(removed' tests/cli/replace-live-disk.py tests/cli/replace-live-disk.nix
   ```
   Expect zero matches.
2. **New prose matches the body** -- re-read both preambles against the Claim
   audit list above; every claim must trace to a subtest or to `replace.rs`.
3. **Optional, confirmatory only** (not required for a comments-only change; the
   VM suite is 20-30 min): a focused `just test-vm replace-live-disk` confirms the
   wrapper still parses and the test still passes. Skip unless paranoid; report to
   the user that a comment-only edit needs no suite run.
