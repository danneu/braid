# Close the degraded-add balance-skip: make the live execute gate the sole real-run emitter

## Baseline: this plan is a DELTA on the code committed in `1af97ff3`

**Read this before anything else.** The first three quadrants of this fix are
already **committed** as `1af97ff3 fix(add): gate raid1 balance on fresh pool
probe`. This plan's three files -- `cli/src/add.rs`, `cli/src/pool.rs`,
`docs/internals/btrfs/balance-soft.md` -- carry that committed code and are
unchanged since `1af97ff3` (which is no longer HEAD: unrelated commits have landed
on top, but none touch these three files, so HEAD's copies are byte-identical to
the commit's and every `~line` reference below still holds). Inspect the landed
implementation with `git show 1af97ff3` -- do NOT rely on `git diff` /
`git diff --cached`, which say nothing about this plan and may show unrelated work
staged or committed elsewhere. That commit carried `+240` lines in `cli/src/add.rs`
(plus the smaller `pool.rs` and `balance-soft.md` edits) and also promoted its own
predecessor plan to [`plans/impl/2026-06-25-1333-add-fresh-probe-raid1-balance.md`](../impl/2026-06-25-1333-add-fresh-probe-raid1-balance.md)
-- read that for the full rationale of the committed code. Earlier drafts of
*this* plan were written against the pre-commit HEAD and described code that no
longer exists; this version is re-baselined onto `1af97ff3` and scoped to the one
behavior still broken.

**This plan builds on `1af97ff3`; it does not revert it.** The committed code is
correct as far as it goes -- it landed the shared `pool_can_host_raid1` predicate
and three of the four quadrants. The only alternative would be to revert
`1af97ff3` and re-derive from the pre-commit HEAD, which is wrong: it discards the
helper and three landed quadrants to re-litigate settled work. Build on it.

**What this plan supersedes from the committed impl plan.** That plan deliberately
kept the execute skip branch *race-only*
(`self.pool.missing_count == 0 && pool_after.missing_count > 0`) and let the note
*replay* carry the plan-degraded skip, arguing the two are "mutually exclusive, so
[the gate] never double-emits." That model is sound for the three quadrants it
considered, but it never examined the member-returned quadrant (plan degraded ->
live healthy), where the replay fires a stale `[skip]` and the gate then balances.
This plan replaces that sub-decision: the execute gate becomes the *sole* real-run
emitter (widen the branch + filter the replay), which closes the member-returned
contradiction while keeping the plan-degraded skip exactly once. The two edits are
coupled -- see "The fix".

### Already done (committed in `1af97ff3`) -- treat as DONE, do not re-implement

- **Live balance gate.** `AddPlan::execute` (`cli/src/add.rs`) no longer reads
  `self.pool.missing_count`/`total_after` for the balance. It is now
  `if pool_can_host_raid1(&pool_after) { ...balance... }` (~line 1554). The old
  `let total_after = self.pool.devices.len() + mapper_paths.len();` binding is
  gone from `execute`; `total_after` now lives only in the dry-run
  `AddWorkPlan::render_steps` predictor (~line 805), keyed on
  `pre_add_missing_count`.
- **Shared predicate.** `pool_can_host_raid1(pool) -> bool`
  (`missing_count == 0 && devices.len() >= 2`) is defined in `cli/src/pool.rs`
  (~line 443, `pub(crate)`) and used by BOTH the add gate and
  `maybe_restore_raid1` (~line 469), so the two gates cannot drift.
- **Race quadrant (plan healthy -> live degraded) implemented + tested.** The
  gate has a race-only skip branch
  `else if self.pool.missing_count == 0 && pool_after.missing_count > 0` (~line
  1568) that emits the skip note, pinned by
  `execute_newly_degraded_add_skips_raid1_balance_and_emits_note` (~line 4926)
  via a new `degrade_after_add` knob on `RecoverableAddRunner`.
- **Plan-degraded quadrant tested.** `cmd_add_plan_degraded_emits_balance_skip_once`
  (~line 5983) pins skip-exactly-once for plan-degraded -> live-degraded;
  `execute_degraded_add_skips_raid1_balance` (~line 4839) covers the same
  quadrant at the `execute` level (no skip assertion).
- **`balance-soft.md` "Skip" paragraph** already rewritten to the
  advisory-plan / authoritative-execute description.

### The one remaining gap -- the entire scope of this plan

The member-returned quadrant (**plan degraded -> live healthy**) is still broken
and **untested**:

- `execute` replays ALL accumulated notes unfiltered at the top, before any
  mutation: `preview::emit_notes_to_stderr(&self.notes, ...)` (~line 1022,
  untouched by `1af97ff3`).
- A degraded `plan_add` pushes a `PreviewNote::Skip` (~line 1896). So when the
  pool was degraded at plan time but the missing member is back by the post-add
  probe, `execute` prints `[skip] pool: RAID1 balance skipped` from the replay
  **and then runs the balance** (the live gate sees a healthy pool). A
  self-contradictory transcript -- "skipped" immediately followed by balancing --
  and a real, live, unguarded defect.

Closing it is a two-line code change plus one new test (details below).

### Why this is principled, not novel

The end state extends an already-established pattern -- execute re-validates live
state that planning could not -- documented across the authority docs:

- [`safety-heuristics.md`](../../docs/dev/safety-heuristics.md): *"Query the
  authoritative source of state directly; do not pre-gate it with a cheaper but
  weaker observable."* The plan-time `missing_count` is that weaker observable;
  `pool_after` is the authoritative source. The same principle is why the skip
  decision must have a single source (the gate), not two (gate + replay).
- [ADR 022 dry-run-preview-model](../../docs/design/decisions/022-dry-run-preview-model.md):
  execute "may still perform execution-time validation that dry-run
  intentionally cannot do." The plan-time `PreviewNote::Skip` is a dry-run
  prediction; the real-run skip is an execution-time decision.

## Decision (confirmed): live state is authoritative; the gate is the sole real-run emitter

The execute gate already decides the balance purely from `pool_after`. This plan
finishes the model: **the execute gate is the sole emitter of the real-run
balance-skip line**, for every degraded outcome. The plan-time `PreviewNote::Skip`
drives only the dry-run preview prediction and is **filtered out of the real-run
note replay**. So the real-run **balance-skip line** is a pure function of the
live pool -- that line and the live balance action can never contradict each
other.

| Plan-time pool | Live pool (`pool_after`) | Real-run outcome | Status |
| --- | --- | --- | --- |
| healthy, >=2 after add | healthy, >=2 present | **balance**; no skip line | done (`execute_fresh_..._balances_once`) |
| healthy | degraded (member dropped) | **skip** -- gate emits | done (`execute_newly_degraded_...`) |
| degraded | degraded | **skip** -- gate emits (replay no longer needed) | done test (`cmd_add_plan_degraded_...`); **moves replay->gate this plan** |
| degraded | healthy (member returned) | **balance**; **no skip** -- note filtered, gate quiet | **this plan's fix + Test B** |
| any | < 2 present, 0 missing | no balance, silent | gate `else` falls through |

The missing-devices `PreviewNote::Warn` is a separate plan-time note. It still
replays unchanged and is *not* re-decided here; in the member-returned quadrant it
can be stale, but unlike the Skip it gates nothing and contradicts no live action,
so it is out of scope (pre-existing -- see "Out of scope").

The preview remains a plan-time prediction and may legitimately diverge from
execute when pool health changes mid-operation (member drops OR returns) --
permitted by ADR 022.

## The fix -- two coupled edits in `cli/src/add.rs` (land together)

These two edits are **coupled and must land in the same change**. Applying the
filter (Edit 1) without widening the gate (Edit 2) would silently regress the
plan-degraded -> live-degraded quadrant: today its skip comes *solely* from the
replay, so filtering it out with the gate's skip branch still race-only would
emit **no** skip there at all, turning `cmd_add_plan_degraded_emits_balance_skip_once`
red and leaving real operators with no explanation.

### Edit 1. Filter the plan-time Skip note out of the real-run replay (`AddPlan::execute`, ~line 1022)

The top-of-`execute` replay currently emits every accumulated note before
mutation. The degraded-balance `PreviewNote::Skip` is a plan-time **prediction**;
on the real run the gate (Edit 2) is the authoritative emitter. Replaying it too
either double-emits (live degraded) or -- the bug this plan fixes -- prints
"balance skipped" and then balances (live healthy, member returned). Filter it:

```rust
// The degraded-balance PreviewNote::Skip is a DRY-RUN prediction only; the
// execute balance gate below is the sole real-run emitter, keyed on the live
// pool_after probe. Replaying it here would contradict the gate when a missing
// member returned (note says "skipped", gate balances) or duplicate it when the
// pool is still degraded. add is the only PreviewNote::Skip producer (preview.rs).
let replay_notes: Vec<PreviewNote> = self
    .notes
    .iter()
    .filter(|n| !matches!(n, PreviewNote::Skip(_)))
    .cloned()
    .collect();
preview::emit_notes_to_stderr(&replay_notes, PerDiskStyle::Bracketed);
```

The variant filter is precise: `preview.rs`'s `PreviewNote::Skip` doc records
"Only `add`'s degraded-balance path produces it today." Warn/Info notes are not
re-decided at execute and replay unchanged -- exactly today's behavior. The
`cmd_add` refusal-path replay needs no filter: the Skip is pushed only on
`plan_add`'s success path, so a `PlanFailure` never carries it.

### Edit 2. Widen the gate's skip branch to all live-degraded pools (`AddPlan::execute`, ~line 1568)

The committed gate's skip branch is **race-only**:

```rust
} else if self.pool.missing_count == 0 && pool_after.missing_count > 0 {   // race-only
    preview::emit_notes_to_stderr(&[PreviewNote::Skip(format_add_degraded_balance_skip())], PerDiskStyle::Bracketed);
}
```

Widen its condition so the gate emits the skip for **any** live-degraded pool,
making it the sole real-run emitter for both degraded quadrants (the change is the
`else if` condition only -- the emission body is unchanged):

```rust
} else if pool_after.missing_count > 0 {
    // Live pool is degraded: skip the hard convert and announce it. This single
    // branch covers BOTH the race (plan healthy, member dropped mid-op) and the
    // genuinely-degraded add (plan degraded, member still missing). The plan-time
    // PreviewNote::Skip is filtered from the replay (Edit 1), so this is the one
    // and only real-run skip emission -- no double-emit, no stale "skipped" before
    // a balance. See docs/internals/btrfs/balance-soft.md.
    preview::emit_notes_to_stderr(
        &[PreviewNote::Skip(format_add_degraded_balance_skip())],
        PerDiskStyle::Bracketed,
    );
}
// else (pool_after.missing_count == 0 but devices.len() < 2): no balance and no
// skip body -- with no missing device the "still has a missing device" line would
// be false. Degenerate (the add did not yield >=2 present); stay silent.
```

**Predicate note (a correction worth stating).** The condition is
`pool_after.missing_count > 0`, NOT `pool_after.devices.len() >= 2`. The race
test's post-add probe has only **one** present device (`degrade_after_add` reports
`devid 1 MISSING` + the just-added disk2 = 1 present, 1 missing), so a
`devices.len() >= 2` floor would stop the gate firing there and break
`execute_newly_degraded_add_...`. `missing_count > 0` is also the accurate
trigger for the body text ("pool still has a missing device; redundancy not
restored"), which is true whenever a member is missing regardless of present
count.

**Reuse, do not reinvent (`pool_can_host_raid1`).** The balance arm already calls
`pool_can_host_raid1(&pool_after)` -- keep it; do not introduce a parallel inline
`live_healthy`. The skip arm is the complementary degraded check
(`pool_after.missing_count > 0`), not a second copy of the host-RAID1 predicate.
The emission helper `format_add_degraded_balance_skip()` and the
`emit_notes_to_stderr(&[PreviewNote::Skip(...)])` form are the committed tree's, kept
as-is (they route through `emit_status`, so `capture_with_color` observes them).

### One emission site for the balance-skip line

After Edits 1-2 the real-run balance-skip line has exactly one source: the live
gate. In every quadrant the emitted skip line matches the balance action -- the
degraded paths emit one skip, the balance paths emit none, and the member-returned
case can no longer print a stale "skipped" before balancing. (Invariant scoped to
the balance-skip line; the missing-devices Warn still replays unchanged, gates
nothing, and is out of scope.)

## Tests (Rust unit, `cli/src/add.rs` `mod tests`)

VM coverage is impossible: the degraded VM tests pre-synthesize the missing device
before `braid add` runs, and the prompt window is synchronous -- no disk can be
pulled or returned mid-prompt. Unit is the only lane.

### Already covered by the committed tree -- do NOT re-add

- **Plan healthy -> live degraded (race):** `execute_newly_degraded_add_skips_raid1_balance_and_emits_note`
  (~line 4926). This is the old plan's "Test A"; it already exists under a
  different name and runner. Do not add a second one.
- **Plan degraded -> live degraded:** `cmd_add_plan_degraded_emits_balance_skip_once`
  (~line 5983, asserts skip-exactly-once) and
  `execute_degraded_add_skips_raid1_balance` (~line 4839, balance==0 +
  device-add==1). The old plan's "strengthen the degraded test" item is redundant
  -- skip-exactly-once is already asserted at the `cmd_add` level. Do not add it.
- **Plan healthy -> live healthy:** `execute_fresh_add_to_mounted_single_device_pool_balances_once`
  (~line 4761, balance==1).

### New: Test B -- the missing member reappears before the balance (the only new test)

The direct regression guard for this plan's fix. **No new runner is needed**
(this drops the old plan's `MemberMoveAddRunner`): the member-returned asymmetry
falls straight out of a degraded *planned* pool run against the normal
*healthy-probe* runner.

- Planned `PoolState`: DEGRADED -- reuse the literal from
  `execute_degraded_add_skips_raid1_balance` exactly: `devices: [disk1]`,
  `missing_count: 1`, `missing_devids: [Devid::new(2)]`, `total_devices: 2`,
  `fsid: Some(POOL_FSID)`.
- Notes: push **both** plan-time notes a degraded `plan_add` accumulates -- the
  missing-devices Warn first (~line 1832), then the degraded-balance Skip (~line
  1896) -- after building the plan (`notes` is `pub`):
  ```rust
  plan.notes.push(PreviewNote::Warn(format_add_missing_devices_warning(1)));
  plan.notes.push(PreviewNote::Skip(format_add_degraded_balance_skip()));
  ```
  The Skip is what made the design contradict itself; the Warn is there so the
  test also proves the filter is `Skip`-only (a non-Skip note must survive).
- Target: `fresh_target("disk2", "/dev/disk/by-id/virtio-disk2", "2222...")` --
  the same disk2/devid-2 target as the degraded test. Adding disk2 fills the
  missing devid-2 slot, which is exactly "the missing member is back" as far as
  the gate's `pool_after` is concerned.
- Runner: **`RecoverableAddRunner::new()`** -- the normal, non-degraded,
  non-`degrade_after_add` runner. Its post-add probe reports disk1 + disk2 present,
  0 missing (the same healthy probe `execute_fresh_..._balances_once` relies on),
  so `pool_can_host_raid1(&pool_after)` is true and the gate balances.
- Capture pattern: the established `capture_with_color(false, || { result =
  Some(plan.execute(...)); })` wrapper (as in `execute_newly_degraded_add_...`).
- Assert: `result.is_ok()`; exactly one `CmdRequest::BtrfsBalanceRaid1`; exactly
  one `CmdRequest::BtrfsDeviceAdd`; `captured` does **NOT** contain the
  balance-skip body; `captured` **DOES** contain the missing-devices Warn body
  (pinning the filter is `Skip`-only and leaves non-Skip notes replayed). Use the
  formatter helpers for both bodies, not hardcoded literals (see Finding-6 note).
- Fail-first: against the current committed tree (no filter) the replay prints the
  stale Skip body, so the skip-absent assertion **fails** -- the exact live bug.
  The balance count is already 1 in the committed tree (the gate is live), so the
  balance assertion does not fail-first; the skip-absent assertion is the guard.
  The Warn-present assertion is invariant (Warn replay is unchanged before/after).

### Re-confirm green after Edits 1-2 (no new tests, just verify)

The two edits move the plan-degraded skip from the replay to the gate; verify the
existing tests still pass:

- `cmd_add_plan_degraded_emits_balance_skip_once` (~5983): with the filter, the
  replay no longer emits the Skip; with the widened gate (`missing_count > 0`),
  the gate emits it once. Net: still exactly one skip. Must stay green.
- `execute_newly_degraded_add_...` (~4926): the widen broadens the race-only
  condition to all live-degraded; this case (1 present, 1 missing) still matches
  (`missing_count > 0`), so the skip still fires. Must stay green. (This is why
  the predicate is `missing_count > 0`, not `devices.len() >= 2`.)
- `execute_degraded_add_...` (~4839) and `execute_fresh_..._balances_once`
  (~4761): unaffected (no skip assertion / healthy probe). Must stay green.

### Finding-6 note: skip-body assertion convention

The committed tests assert the literal prefix `"[skip] pool: RAID1 balance skipped"`,
while `format_add_degraded_balance_skip()` returns a longer two-sentence body.
Test B should assert against the **helper** (`format_add_degraded_balance_skip()`
/ `format_add_missing_devices_warning(1)`), not a hardcoded string, so a body
reword cannot silently desync it. Optionally align the existing committed tests to
the helper in the same change for one convention; low priority, severable.

## Docs

Most of the committed `balance-soft.md` rewrite and the committed comment updates
(`pre_add_missing_count` field doc, `render_steps` comment, the execute-gate
comment) already match the end state. The remaining touch-ups are only those the
filter/widen actually invert:

**Required (filter-/widen-necessitated):**

- **`AddPlan` struct doc (`cli/src/add.rs`, ~line 981-986).** It says "`execute()`
  renders the accumulated notes to stderr ... before any mutation." Add the
  exception: the degraded-balance `PreviewNote::Skip` is filtered from the replay
  and re-emitted by the live balance gate; Warn/Info notes replay unchanged.
- **`plan_add` push comment (`cli/src/add.rs`, ~line 1881-1890, committed version).**
  It currently says the execute gate "may emit the same note only when the plan
  was healthy and the fresh post-add probe is newly degraded, so the two branches
  stay mutually exclusive." After this plan that is wrong: the plan-time Skip is
  dry-run-only (filtered from the real-run replay), and the gate is the sole
  real-run emitter for **all** degraded outcomes (not just the newly-degraded
  race). Rewrite to that.
- **`balance-soft.md` "Skip" paragraph (~line 87-95).** The committed text frames the
  divergence as one-directional ("when a pool member drops after planning ... the
  real run skips"). Update to: the plan-time note is a dry-run prediction only;
  the real-run skip is emitted solely by the execute gate from `pool_after`, which
  diverges from the preview in **both** directions -- a member that drops is
  skipped (and announced) and a member that returns is balanced (the previewed
  skip is suppressed).

**Not needed (the committed emission form keeps them accurate):** the
`PreviewNote::Skip` doc in `preview.rs` (the gate still emits a `PreviewNote::Skip`
via `emit_notes_to_stderr`, so "rendered in both dry-run stdout and real-run
stderr" stays true) and `format_add_degraded_balance_skip`'s doc (still the single
body source). Do not touch them.

**Optional / opportunistic (pre-existing, severable, unrelated to this fix):**
`balance-soft.md` ~line 104-107 claims "The soft convert, by contrast, is left
running even on a degraded pool." That is false for one of the two soft callers:
`maybe_restore_raid1` runs the soft balance **only after** the last missing device
clears (`pool_can_host_raid1(&pool_after)`) and emits its own skip while still
degraded -- it does NOT run on a degraded pool; only `recover`'s
`replay_owed_raid1_maintenance` may run on a still-degraded pool. Both are safe
because `,soft` only converts `single` -> `raid1`. Splitting the claim is a
correct doc fix but orthogonal to the member-returned defect; include it only if
touching this file anyway.

## Out of scope / non-goals

- **Reverting `1af97ff3`.** This plan builds on it. Reverting loses
  `pool_can_host_raid1` and three landed quadrants.
- **The preview/`plan_add` Skip stays plan-time** -- a preview cannot see the
  future; ADR 022 sanctions the divergence.
- **The missing-devices `PreviewNote::Warn` replay is untouched.** It is a
  plan-time health observation that gates and contradicts no execute-time action,
  so it cannot produce a self-contradictory transcript. In the member-returned
  quadrant the replayed Warn ("pool has 1 missing device") is stale, but that is
  pre-existing (the committed tree already replays it there) and informational, not
  an action mismatch. Re-deriving the warning from `pool_after` is a separate
  concern. Test B's Warn-present assertion documents the boundary, it does not
  endorse the staleness.
- **The soft-convert paths are out of scope** (different *when*; both safe via
  `,soft`). See the optional doc note above.
- **No new test runner.** The old `MemberMoveAddRunner` is dropped; the normal
  `RecoverableAddRunner` plus a degraded planned pool covers quadrant 4.
- **No abort/fail-closed gate.** A degraded add is supported; the fix downgrades
  the plan (skip the balance), it does not refuse the add.
- **No parser/tool-version change** -> no fixture refresh.

## Verification

1. `just test-rust`: Test B fails first for the right reason against the committed
   tree (the unfiltered replay prints the stale skip body), then passes after
   Edits 1-2. The four existing quadrant tests
   (`execute_newly_degraded_add_...`, `cmd_add_plan_degraded_...`,
   `execute_degraded_add_...`, `execute_fresh_..._balances_once`) stay green --
   especially `cmd_add_plan_degraded_...` (skip moves replay->gate) and
   `execute_newly_degraded_add_...` (the widen must keep it firing at one present
   device). All other add tests stay green.
2. `just clippy` (`cargo clippy --manifest-path cli/Cargo.toml --tests`, lints the
   new test) and `just check-output-ascii` both clean -- the skip body is the
   existing ASCII helper.
3. `just docs-build` -- mdBook + linkcheck pass after the `balance-soft.md` edits.
4. Existing E2E `tests/cli/add-degraded-then-remove-missing.py` and
   `tests/cli/braid-add-warnings.py` (Phase 3) still pass -- they cover the
   already-degraded path, which is behavior-preserved.
