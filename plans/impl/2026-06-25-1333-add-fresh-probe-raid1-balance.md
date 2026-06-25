# Plan: gate `braid add`'s post-add RAID1 balance on the fresh probe

## Context

`braid add`'s execute path decides whether to run the hard
`btrfs balance -dconvert=raid1` (`pool_balance_raid1`, which has **no degraded
guard of its own**) from the **plan-time** snapshot `self.pool.missing_count`
(in `AddPlan::execute`, `cli/src/add.rs` ~line 1560). Between planning and that
gate there is a window -- operator confirmation, passphrase entry, and Pass-2
LUKS format/open of fresh disks -- during which a pre-existing pool member can
drop to `missing` (or be hot-unplugged). Because the gate reads the stale
snapshot, `add` would run a hard, full-chunk RAID1 convert against a
now-degraded pool: exactly the case the missing-count guard exists to prevent.

The fix: decide the balance on the **freshest probe already in hand** --
`pool_after` (`cli/src/add.rs` ~line 1514), taken *after* the device adds,
immediately before the balance -- and emit the degraded-balance-skip note at
execute time when that fresh probe (not the plan) reveals a newly-missing
member.

This brings `add` in line with the established pattern. `replace` and
`remove-missing` already route their post-op balance through
`maybe_restore_raid1` (`cli/src/pool.rs#maybe_restore_raid1`), which re-probes
and decides on `pool_after.missing_count == 0 && pool_after.devices.len() >= 2`.
`add` is the lone command that reimplements the balance decision inline against
a stale count. (The original finding suggested `fresh_pool` at line 1227 and
claimed "the same pattern exists in `replace`" -- both are off: `replace` is the
*correct* reference, and `fresh_pool` is probed at execute-start, *before* Pass-2
and the adds, so it misses a member that drops during those phases. `pool_after`
is the last probe before the balance and closes the window completely.)

Why `pool_after` is authoritative and safe: the post-condition loop
(`cli/src/add.rs` ~lines 1529-1538) already proves every newly-added target is
present in `pool_after`, so `pool_after.missing_count > 0` can only mean a
pre-existing member went missing. Probe semantics confirm the check: `devices`
excludes missing/hot-unplugged members and `missing_count = total_devices -
devices.len()` (`cli/src/probe.rs#probe_pool`), so `missing_count == 0` strictly
implies a fully healthy pool, and the count also catches hot-unplugged
(`null_underlying`) members.

Intended outcome: `braid add` never runs a hard RAID1 convert against a pool
that became degraded after planning, and the operator -- who was told the
balance would run -- is told why it was skipped.

**Reconciliation with ADR 022 (dry-run-preview-model).** ADR 022 forbids
`execute()` from rediscovering or reinterpreting semantic choices made during
planning, but explicitly permits "execution-time validation that dry-run
intentionally cannot do." This change is that carve-out, not the prohibition:
`add`'s `execute()` has *always* computed this balance gate inline (it was never
a cached `Step` or a `WorkPlan` flag); we change only its *input* -- from the
stale plan-time snapshot to an authoritative live re-probe (`pool_after`) that
the planner could not have observed, since the member drops *after* planning. It
replicates the already-blessed advisory-plan / authoritative-execute split that
`replace`/`remove-missing` use (`should_restore_raid1` plan-time advisory +
`maybe_restore_raid1` execute-time re-probe). ADR 022 needs no normative edit;
the preview deliberately remains a best-effort predictor that may diverge from
the real run when live state changes after planning.

## Approach

### 1. Shared RAID1-feasibility predicate (`cli/src/pool.rs`)

The execute gate's fresh-probe check is byte-identical to the one already inside
`maybe_restore_raid1` (`cli/src/pool.rs:462`). This is genuine duplication --
both sites ask the same topology question and would change together. Extract it
into a named predicate, sibling to the existing plan-time advisory
`should_restore_raid1`:

```rust
/// Authoritative post-probe check: does the live pool currently have at least
/// two present members and none missing -- i.e. can it hold a full RAID1 layout
/// right now? Both `add`'s grow-balance gate and `maybe_restore_raid1`'s restore
/// gate decide on this exact topology fact, so they share one source and cannot
/// drift. Execute-time/authoritative counterpart to the plan-time advisory
/// `should_restore_raid1`.
pub(crate) fn pool_can_host_raid1(pool: &PoolState) -> bool {
    pool.missing_count == 0 && pool.devices.len() >= 2
}
```

Route the inline check in `maybe_restore_raid1` through it (behavior-preserving;
covered by `maybe_restore_raid1_runs_soft_balance`, `_skips_when_still_degraded`,
`_skips_single_device` in `cli/src/pool.rs`).

### 2. Execute gate + execute-time skip note (`cli/src/add.rs`, `AddPlan::execute`)

Replace the stale gate (currently ~lines 1550-1574). Drop the `total_after`
binding -- it is used only here, and `pool_after.devices.len()` is the real
post-add count:

```rust
if pool::pool_can_host_raid1(&pool_after) {
    eprint!("{}", status_line(StatusTag::Wait, color_enabled, "pool: balancing to RAID1..."));
    pool_balance_raid1(runner, mount_point, params.progress)?;
    eprint!("{}", status_line(StatusTag::Ok, color_enabled, "pool: RAID1 balance complete"));
} else if self.pool.missing_count == 0 && pool_after.missing_count > 0 {
    // Newly degraded since planning: the plan-time PreviewNote::Skip did not fire
    // (the plan saw a healthy pool), so surface the same note now so an operator
    // who was told "balance to RAID1" learns why it was skipped. Mutually
    // exclusive with the plan-time note (which requires pre_add_missing_count > 0),
    // so this never double-emits.
    preview::emit_notes_to_stderr(
        &[PreviewNote::Skip(format_add_degraded_balance_skip())],
        PerDiskStyle::Bracketed,
    );
}
```

All symbols (`preview`, `PreviewNote`, `PerDiskStyle`, `status_line`,
`StatusTag`, `pool::*`, `format_add_degraded_balance_skip`) are already in scope
in `add.rs`. The skip-note body is unchanged (single source at
`cli/src/add.rs#format_add_degraded_balance_skip`), so plan-predicted and
execute-discovered skips render identically.

### 3. Update the "lockstep" comments (`cli/src/add.rs`)

Four comment blocks currently assert that the plan-time count and the execute
gate are identical/in-lockstep. Revise them to state the new, correct contract:
the plan-time count (`pre_add_missing_count` / `self.pool.missing_count`) is a
best-effort **predictor** for the dry-run preview; the execute gate is
**authoritative** on the fresh post-add probe; and the execute path emits the
skip note only in the mutually-exclusive newly-degraded case.

- `pre_add_missing_count` doc comment (~lines 592-596)
- plan-time balance step comment (~lines 806-813)
- execute gate comment (~lines 1551-1559) -- rewritten as part of step 2
- plan-time skip-note comment, "The execute gate ... must NOT also print this,
  or it double-emits" (~lines 1885-1890)

### 4. Docs

- `docs/internals/btrfs/balance-soft.md`, "Skip -- degraded add" section: the
  load-bearing sentence currently asserts the preview step builder
  (`AddWorkPlan::render_steps`) and the execute balance gate (`AddPlan::execute`)
  "both carry the same `missing_count == 0` condition so dry-run and real-run
  agree." This change deliberately **breaks that symmetry** -- do not leave a
  vague "decided at plan time" edit that keeps the false "they agree" claim.
  Rewrite that paragraph to the asymmetric contract: `plan_add`/`render_steps`
  key on the plan-time `pre_add_missing_count` as a best-effort **predictor** for
  the preview; the execute gate is **authoritative** on the fresh post-add probe
  (`pool_after.missing_count`); the two may diverge when a member drops after
  planning, and in that case the real run skips the convert and surfaces the same
  `[skip]` note so the operator is told. Note this is the same advisory-plan /
  authoritative-execute split as `should_restore_raid1` + `maybe_restore_raid1`,
  and the ADR 022 execution-time-validation carve-out (not forbidden
  rediscovery). The convergence paragraph ("Skipping at add also makes the
  degraded-add interrupt paths converge") stays accurate -- the newly-degraded
  path lands in the same deferred-repair end-state -- and needs no change.
- `docs/design/decisions/022-dry-run-preview-model.md`: **no normative edit
  needed** -- the change fits the existing "execution-time validation that
  dry-run intentionally cannot do" carve-out (the Context section above records
  the reconciliation). Confirm during implementation that nothing in ADR 022
  asserts add's balance gate is plan-time-symmetric.
- Sanity-scan `docs/dev/safety-heuristics.md` and `docs/design/principles.md`
  for the "never silently degraded" invariant -- the change *strengthens* it
  (operator still gets the skip note); no contradiction expected, so likely no
  edit beyond the balance-soft note.

## Tests (`cli/src/add.rs` test module)

**Test A -- newly-degraded skip + note** (new). The pool is **healthy at plan
time and degraded only after the device add**, so the skip note originates from
the new execute-time branch (the plan carries none).

- **Scenario:** 1-device pool (disk1) healthy at plan; `execute` adds disk2;
  the post-add `BtrfsFilesystemShow` reports disk2 present and disk1 as `MISSING`
  (`missing_count == 1`), while the pre-add probe and the planned `PoolState`
  are healthy (`missing_count: 0`).
- **Harness:** direct `plan.execute` via `plan_for_execute_target` (which yields
  `notes: vec![]` and lets us pass a healthy `PoolState`), with
  `RecoverableAddRunner` extended by a `degrade_after_add` mode (e.g. constructor
  `degrades_after_add()`). `pool_show()` already keys on `disk2_added` (flipped
  on `BtrfsDeviceAdd`); in the new mode, once `disk2_added` is set, render disk1
  as a `path MISSING` line (devid 1) **instead of** its present line -- replace,
  do **not** append (the runner currently hardcodes disk1 present, and the
  existing always-on `degraded` flag *appends* an extra missing devid), keeping
  disk2 present (`Total devices 2`, present = disk2, missing = disk1). The planned
  `PoolState` passed to `execute` stays healthy. Copy the `path MISSING` line
  literal (`devid <n> size 0 used 0 path MISSING`) from
  `AddPlanTestRunner::with_missing`, but reuse only its *format*: that helper
  *appends* placeholder devids on top of the present set (the exact append pattern
  to avoid here), so Test A takes the line, not the topology.
- **Assert** (wrap runner in `RequestRecordingRunner`; capture stderr via
  `crate::status_tag::testing::capture_with_color(false, || ...)`):
  - `BtrfsBalanceRaid1` issued count == 0,
  - the `format_add_degraded_balance_skip()` body appears in captured stderr
    **exactly once** (here it comes solely from the execute-time branch),
  - result is `Ok` (the add itself succeeds; degradation skips only the balance).

**Test B -- plan-degraded double-emit guard** (new, end-to-end). Guards the
"mutually exclusive, never double-emits" invariant on the *real* production path:
on a plan-degraded pool the skip note is accumulated by `plan_add` into
`plan.notes` and emitted once at execute start (`emit_notes_to_stderr(&self.notes,
...)`, `cli/src/add.rs:1024`), and the new execute-time branch must stay dead
(`self.pool.missing_count > 0`). Do **not** hand-seed `plan.notes` into a
`plan_for_execute_target` plan -- that reconstructs the note's origin and tests a
fiction.

- **Harness:** drive the full path via `cmd_add` (which runs `plan_add` ->
  `execute`) using the cmd_add-level fixture `AddFullPathRunner`, configured to
  read **degraded** (one missing member) at both plan and execute. If
  `AddFullPathRunner` has no degraded builder yet, add one mirroring
  `RecoverableAddRunner::degraded()`'s `path MISSING` rendering
  (`AddPlanTestRunner::with_missing` is the other ready template -- its
  append-a-missing-placeholder style is correct here, since the pool is genuinely
  degraded at both plan and execute). Non-dry-run; wrap in
  `RequestRecordingRunner`; capture stderr with `capture_with_color`.
- **Assert:** the `format_add_degraded_balance_skip()` body appears in captured
  stderr **exactly once**, and `BtrfsBalanceRaid1` issued count == 0. A future
  regression that let the execute-time branch fire on a plan-degraded pool would
  surface here as a second copy.

Regression guards (should stay green):
- `execute_fresh_add_to_mounted_single_device_pool_balances_once` -- healthy ->
  balance runs once (now decided via `pool_can_host_raid1(&pool_after)`).
- `execute_degraded_add_skips_raid1_balance` -- plan-degraded -> no balance. Keep
  **as-is**: it builds its plan via `plan_for_execute_target` (`notes: vec![]`),
  so it guards the balance-count behavior, not note emission. Do **not** add a
  note-count assertion -- with empty `self.notes` and `self.pool.missing_count ==
  1` the note appears *zero* times there, so "exactly once" would fail and "zero"
  would guard nothing. Double-emit is covered by Test B.
- `plan_add_degraded_preview_omits_balance_step` -- dry-run preview unaffected
  (still asserts the preview surfaces the skip note exactly once).

## Verification

- `just test-rust` (or `cargo test -p braid -- add:: pool::`) -- new + regression
  unit tests.
- `cargo clippy` -- confirm dropping `total_after` leaves no unused-binding or
  dead-code warning.
- `just docs-build` -- mdbook linkcheck passes for any doc edits.
- No VM/integration test needed: the gate logic is fully observable through the
  mock runner (issued commands + captured stderr); there is no new integration
  surface.
