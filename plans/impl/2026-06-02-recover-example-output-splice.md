# Fix the impossible `braid recover` "Basic example" output splice

## Context

`docs/commands/recover.md:21-30` shows a `braid recover` output block that the
tool can never emit. It splices stderr from two mutually-exclusive code paths:

- The `pre-operation membership:` / `target membership:` / `recovered (live pool):`
  triple and the `note:` line come **only** from `execute_generic_live_pool_recovery`
  (`cli/src/recover.rs:1133-1148`), reached only by **bootstrap add** and **plain
  remove** (dispatch `recover.rs:1519-1538`).
- The two `pool.json written from completed add membership.` /
  `pool.json written from committed add membership.` lines come **only** from the
  **non-bootstrap add** path (`execute_add_pool_mutation_recovery` ->
  `execute_add_post_balance_recovery`, `recover.rs:2606` + `2265`), which never
  prints the triple.

The example's pre-membership `{"ironwolf", "toshiba"}` is non-empty, so
`is_bootstrap_add()` is false (`cli/src/journal.rs:37`) and it routes to
`RecoverCompletion::AddPoolMutation` (`recover.rs:1455-1468`) -- the path that
prints the two `pool.json` lines and **no** triple. The block is therefore
impossible, and an operator comparing real output to the doc would think
recovery misbehaved.

This block has drifted **twice**: it was originally fabricated, then commit
`c6963885` ("docs(recover): sync basic example...") correctly fixed the banner
and split the single write line into the `completed`/`committed` pair, but left
the triple + `note:` line in place -- producing the current splice. Each line is
individually real (each appears in *some* path), which is why a line-by-line
sync missed that the triple belongs to a different branch. No test ties the
example block to the real sequenced output, so nothing caught it.

**Scope note:** this is the *only* drifted block. An audit of `docs/` found no
sibling instances -- `recovery-scenarios.md` has no output blocks, and
`discover.md`'s output block is single-path and accurate. So this is a
one-block doc fix plus a focused test guard, not a sweep.

## Decision (chosen with the user)

1. **Show the non-bootstrap add scenario, terse.** Keeps the existing add
   narrative; every shown line is already individually test-pinned, so it is the
   most drift-resistant choice. (The richer remove/bootstrap output with the
   membership triple was the alternative; declined in favor of representativeness
   + test-backing.)
2. **Add a runtime regression guard** so the `AddPoolMutation` stderr contract
   (the skeleton the doc mirrors) is regression-tested and the splice cannot
   reappear in a recover run. This guards code output, not the doc text (see
   Change 2).

## Change 1 -- Correct the doc example (`docs/commands/recover.md:21-30`)

Replace the fenced block (lines 21-30) with the real `AddPoolMutation` output:

```
Recovering from interrupted "add" operation (started 2026-03-15T14:30:00Z)...
pool.json written from completed add membership.
pool.json written from committed add membership.
pending-op.json cleared. Recovery complete.
```

Then add one honest caveat directly under the block, so the terse example does
not read as the *complete* stderr. A non-bootstrap add recovery has two omitted
regions, and the first is **state-dependent** -- the caveat must not present it
as unconditional:

- **Before the `pool.json` lines (state-dependent).** If the pool was offline --
  the typical post-crash case the example narrates, where recover opens LUKS and
  prompts for the passphrase -- recover prints per-disk LUKS-open/mount rows. If
  the pool was *already mounted*, `plan_open_pool_inner` returns early
  (`mount.rs:208-217`) and prints a single `pool already mounted at <mount_point>`
  row (`mount.rs:124-125`, an `Info` note) instead, with **no** per-disk rows.
  (The canonical `recover-add-mixed-batch` test keeps `/mnt/storage` mounted
  through `braid recover`, so its real stderr shows the already-mounted row, not
  per-disk rows.)
- **After the `committed` line (always present).** A RAID1 soft-balance replay
  row pair, *before* the final `pending-op.json cleared` line. Guaranteed: the
  post-add pool always has >= 2 devices, so `replay_owed_raid1_maintenance`
  (`recover.rs:1830-1851`, called at `recover.rs:2279`) always runs the soft
  balance between `recover.rs:2265` and `2287`.

> Before the `pool.json` lines, a real run prints either per-disk LUKS-open and
> mount rows (if the pool was offline) or a single `pool already mounted at ...`
> row (if it was already mounted). After the `committed` line it always prints a
> RAID1 soft-balance replay row pair before the final `pending-op.json cleared`
> line.

Rationale for each kept/dropped line (verified byte-for-byte against the code):

- Banner: `format_recover_entry` (`recover.rs:1181-1186`) uses `{:?}` on the
  lowercase op label -> `Recovering from interrupted "add" operation (...)...`.
  Keep the existing `2026-03-15T14:30:00Z` timestamp for continuity.
- `pool.json written from completed add membership.` -- `recover.rs:2606`.
- `pool.json written from committed add membership.` -- `recover.rs:2265`.
- `pending-op.json cleared. Recovery complete.` -- `recover.rs:2287`.
- **Dropped** triple + `note:` line -- emitted only by the generic-live-pool
  path (`recover.rs:1133-1145`), which a non-bootstrap add never reaches.
- **Dropped** `pool.json written from live pool state.` (`recover.rs:1148`) --
  also generic-live-pool-only; the add path writes the `completed`/`committed`
  pair instead.

## Change 2 -- Runtime regression guard for the `AddPoolMutation` stderr contract (`tests/cli/recover-add-mixed-batch.py`)

This guard pins the *code's* recover stderr, not the doc text. It cannot fail if
someone re-edits the doc badly; what it does is stop the splice reappearing in a
real recover run, so the doc -- sourced from this verified contract -- stays
correct.

This test already captures recover stderr into `err` (line 219), pins the
`completed`/`committed` pair plus their ordering (lines 234-243), **and already
pins the soft-balance replay row pair** (`replaying post-add RAID1 soft balance`
-> `RAID1 soft balance replay complete`, lines 222-228). So the omitted-region-2
balance rows the doc caveat mentions are themselves already tested. It is the
canonical pin site and exercises the non-bootstrap add path. Extend the existing
`with subtest("Recover mixed-batch add"):` block to also pin the banner, the
journal-cleared line, the **full row ordering the caveat promises** (`committed`
-> soft-balance wait -> soft-balance ok -> journal-cleared, so the balance pair
can't drift before `committed` or after `cleared`), and the **absence** of the
generic-live-pool triple. Match the file's existing
`assert ... in err, repr(err)` style:

```python
banner = 'Recovering from interrupted "add" operation (started '
assert banner in err, "recover banner line missing, got: " + repr(err)

cleared_line = "pending-op.json cleared. Recovery complete.\n"
assert cleared_line in err, "journal-cleared line missing, got: " + repr(err)
# Pin the full row ordering the doc caveat promises: the RAID1 soft-balance
# replay pair lands between the committed-membership write and the journal clear
# -- not before `committed`, not after `cleared`. soft_replay_wait/soft_replay_ok
# are already defined (lines 222-223); this chain subsumes the standalone
# soft_replay_wait < soft_replay_ok check at line 226.
assert (
    err.find(committed_line)
    < err.find(soft_replay_wait)
    < err.find(soft_replay_ok)
    < err.find(cleared_line)
), "expected committed -> soft-balance replay -> journal-cleared order, got: " + repr(err)

# Runtime regression guard: non-bootstrap add recovery (AddPoolMutation) must
# never emit the generic-live-pool membership triple -- those lines come from
# execute_generic_live_pool_recovery (recover.rs:1133-1145), reached only by
# bootstrap add and plain remove. This pins the code's stderr contract (the
# skeleton docs/commands/recover.md shows); it does not test the doc text.
for triple_line in ("pre-operation membership:", "recovered (live pool):"):
    assert triple_line not in err, (
        f"add recovery must not print generic-live-pool line {triple_line!r}, "
        f"got: {err!r}"
    )
```

Notes:
- The ordering chain reuses names already defined in the subtest --
  `committed_line` (line 235) and `soft_replay_wait`/`soft_replay_ok`
  (lines 222-223) -- so nothing is re-declared. The chain makes the pre-existing
  standalone `soft_replay_wait < soft_replay_ok` assertion (line 226) redundant;
  leave it (harmless) or collapse it into the chain -- implementer's choice.
- Chained `err.find(a) < err.find(b) < ...` is valid Python and matches the
  file's existing `err.find(x) < err.find(y)` idiom; every term is also asserted
  present (`in err`) so no `find` returns -1.
- The two triple substrings are unambiguous; the `pool.json` lines say
  "...add membership." not "...membership:", so there is no false match. Either
  one alone would suffice; two is for clarity.
- No new test preamble is needed -- these assertions extend an existing subtest.
  No `flake.nix` change (check `recover-add-mixed-batch` already registered,
  `flake.nix:592`).

## Out of scope / intentionally not done

- **No Rust changes.** The banner is already pinned by
  `format_recover_entry_pins_banner_for_each_op_kind` and the guidance strings by
  `guidance_*` unit tests; the code is correct -- only the doc was wrong.
- **No new VM test / no doc-snapshot harness.** Reusing the existing stderr
  capture in `recover-add-mixed-batch.py` is the proportionate guard; a
  full doc-block golden test would be brittle against environment-dependent
  probe/balance rows.
- **No sibling doc edits** -- audit found none drifted.

## Files to modify

- `docs/commands/recover.md` -- replace the example block (lines 21-30) + one
  caveat sentence.
- `tests/cli/recover-add-mixed-batch.py` -- extend the `Recover mixed-batch add`
  subtest (~line 234) with the four guard assertions above.

## Verification

1. **Docs build / linkcheck:** `mdbook build docs` (no links change, but
   confirms the tree still builds clean).
2. **Test guard passes against real output:**
   `just test-vm recover-add-mixed-batch`
   - Confirms the banner, `completed`, `committed`, soft-balance replay pair, and
     `cleared` lines all appear in real recover stderr in the documented order
     (including the balance pair landing between `committed` and `cleared`), and
     the triple is absent -- i.e. the corrected doc block matches reality.
3. **Guard scope (runtime, not doc text):** the new assertions inspect real
   recover stderr only. Reintroducing the triple into the *code's* add path fails
   step 2; editing the doc back to the bad block does **not** fail any test (docs
   aren't snapshot-tested). This is a runtime `AddPoolMutation` regression guard --
   the doc's correctness rests on being sourced from this verified contract plus
   review, not on a test.
