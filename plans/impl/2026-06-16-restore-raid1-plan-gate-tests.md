# Plan: the ideal pivot -- test the restore-RAID1 plan gate end-to-end

## Context

`braid remove-missing` (and `replace`'s missing path) decide whether to run a
post-mutation soft RAID1 rebalance through **two gates**, both keyed on the
deliberately-unified predicate `crate::pool::should_restore_raid1(clears_last_missing,
present_after)` (`pool.rs#should_restore_raid1`, unified in commit `3428f8e1`):

1. **Plan-time gate** -- `plan_remove_missing` computes
   `should_restore_raid1(pool.missing_count == 1, remaining_present)` and stores it as
   the work-plan's `restore_raid1_after_commit` bool (`remove_missing.rs:481`). It is
   persisted into the journal and gates whether `execute` even calls the runtime step
   (`remove_missing.rs:275-288`, which reads the flag back from `post_journal.op`).
2. **Runtime gate** -- `crate::pool::maybe_restore_raid1` (`pool.rs#maybe_restore_raid1`),
   invoked only when the plan flag is `true`, re-probes the real post-state (one
   `BtrfsFilesystemShow`) and runs the balance only if `missing_count == 0 &&
   devices.len() >= 2`; otherwise it skips. Defense in depth: the plan is advisory,
   reality is authoritative.

**The defect.** The test `three_device_two_missing_no_rebalance`
(`remove_missing.rs:1884`) claims in its preamble to model "3-disk pool, 2 missing,
targeting 1 -> NO rebalance", but its fixture
`RemoveMissingPool::three_disk_one_missing().still_degraded_after(true)` reports only
**one** missing devid pre-remove (`missing_count == 1`). So the **plan gate opens**
(`should_restore_raid1(true, 2) == true`), `maybe_restore_raid1` **is** called, and the
balance is suppressed only by the **runtime** re-probe seeing the still-degraded
post-state. The genuine multi-missing case (`missing_count >= 2` pre-remove -> plan gate
**closes** -> `maybe_restore_raid1` never called) has **no** end-to-end coverage: only the
pure render test `work_plan_steps_omit_rebalance_when_not_last_missing`
(`remove_missing.rs:1678`) touches it, and it goes through the test-only constructor
`remove_missing_work_plan_for_test`, never production `plan_remove_missing` -> `execute`.
`replace.rs` shares the same gate and the same gap (it has the render test
`dry_run_missing_not_last_omits_rebalance` at `replace.rs:3144` and the healthy
end-to-end test at `replace.rs:5522`, but no degraded end-to-end test).

**Outcome.** Honestly relabel the mislabeled test, add end-to-end coverage of the
plan-gate-closed path on both commands, and directly pin the persisted journal flag
(the recovery contract) -- the complete, behavioral, structure-insensitive coverage
braid's principles call for.

## Decisions (the ideal options for braid)

- **Journal pin: yes.** The persisted `restore_raid1_after_commit` flag is what `braid
  recover` replays and what `execute` reads to gate the runtime step. Pin it directly via
  a failure-path test (behavioral: `load_journal` + public `OpKind`). Reject the
  white-box in-memory `plan.work_plan.restore_raid1_after_commit` read (structure-sensitive,
  against house style).
- **Mirror to replace.rs: yes.** The gate is unified by design; coverage must be symmetric
  or the replace-side wiring of the shared predicate can regress undetected.

## Changes

### A. `cli/src/remove_missing.rs` -- relabel + tighten the existing test

Rename `three_device_two_missing_no_rebalance` ->
**`runtime_reprobe_vetoes_rebalance_when_pool_still_degraded`**. Keep the
`three_disk_one_missing().still_degraded_after(true)` fixture. Rewrite the
Intent/Why/Scenario preamble to state the real mechanism: the **plan gate opened**
(`missing_count == 1`, queued the balance), but `maybe_restore_raid1`'s authoritative
**runtime re-probe** saw a still-degraded post-state and vetoed -- plan advisory, reality
authoritative. Be precise that the veto is the `pool_after.missing_count == 0` re-probe
check, not the `pre_op_missing_count == 0` early return (which is unreachable here).

Keep the existing assertions (no `BtrfsBalanceRaid1Soft`; inhibitor `acquire_count() ==
1`). **Add** a positive assertion, using the existing `.position(...)` idiom from
`three_device_pool_soft_rebalance_runs` (`remove_missing.rs:1851`): a
`BtrfsFilesystemShow` is recorded at a position **after** the `BtrfsDeviceRemove`
(`show_pos > remove_pos`), proving the runtime re-probe happened. Without this the test
cannot distinguish "vetoed at runtime" from "never attempted." Add a load-bearing comment
tying that post-remove SHOW to the sole gated re-probe site (`maybe_restore_raid1` at
`remove_missing.rs:280`) so a future maintainer reads a break correctly.

### B. `cli/src/remove_missing.rs` -- new test: plan gate closes (success path)

**`two_missing_plan_gate_skips_rebalance_without_reprobe`**. Use the new
`four_disk_devids_pinned()` fixture + `RemoveMissingPool::four_disk_two_missing()`
(below). Run `cmd_remove_missing` targeting devid 3 (the `remove_missing_params()`
default). Assert:
- (a) no `BtrfsBalanceRaid1Soft`;
- (b) **no `BtrfsFilesystemShow` at any position after the `BtrfsDeviceRemove`** -- proves
  `maybe_restore_raid1` was never invoked, i.e. the plan gate closed at plan time (the
  precise discriminator vs. test A). Comment it as coupled to the single gated SHOW site.
- (c) inhibitor `acquire_count() == 1` (invariant holds on this no-balance path too).

### C. `cli/src/remove_missing.rs` -- new test: plan gate persists `false` (failure path)

**`two_missing_journal_persists_restore_raid1_false`**. Same four-disk-two-missing
fixture, but register a per-test handler shadowing `BtrfsDeviceRemove` to return a
nonzero-exit failure. Template: `journal_survives_device_remove_failure`
(`remove_missing.rs:1995`), which returns `exit_status: 1` from the device-remove step --
**not** `journal_survives_soft_balance_failure` (`remove_missing.rs:2282`), which shadows
`BtrfsBalanceRaid1Soft` and leaves the journal advanced to `PostRemoveMissingMaintenance`.
A device-remove failure aborts `execute` before `rewrite_journal` runs, so the surviving
journal is still in `PoolMutation` phase (built with that phase at `remove_missing.rs:221`).
Assert `cmd_remove_missing` errors, then `journal::load_journal(&f.paths)` returns a
surviving journal whose op is `OpKind::RemoveMissing { phase:
RemoveMissingPhase::PoolMutation, restore_raid1_after_commit: false, .. }`. This pins the
persisted recovery artifact directly (the flag `braid recover` would replay), which the
success-path test cannot observe because `execute` clears the journal
(`remove_missing.rs:291`). Doubles as confirming the residual-guard (journal survives a
failed device remove) holds for the multi-missing case.

### D. `cli/src/test_fixtures/remove_missing.rs` -- new fixtures

Add, mirroring the existing `three_disk_*` helpers (each `pub(crate)` item gets a `///`
explaining intent, per AGENTS.md):

- `FOUR_DISK_TWO_MISSING_PRE_SHOW`: **`Total devices 4`**, devid 1 + devid 2 **present**
  (`/dev/mapper/braid-disk{1,2}`), devid 3 + devid 4 **MISSING** rows. Critical:
  present devids must be exactly 1 and 2 -- `probe_pool` resolves every present row through
  `CryptsetupStatus`/`CryptsetupLuksUuid`, and the fixture's `mapper_underlying` /
  `luks_uuid_for_device` only map disk1/2/3; a present devid 4 would fall through to
  `MissingMock` and error the probe. Header `Total devices 4` with exactly 2 present rows
  yields `missing_count == saturating_sub(4, 2) == 2` and `missing_devids == [3, 4]`.
- `usage_raw_four_disk_two_missing()`: `device_usage_raw_body` of
  `remove_missing_usage_live_device(1)`, `remove_missing_usage_live_device(2)`,
  `DeviceUsageSpec::missing(3, &[("Data", "RAID1", 67_108_864)], 0)` (target -- needs a
  positive allocation row so the usage-shape validation passes and survivor capacity is
  checked), and `DeviceUsageSpec::missing(4, ..)` (second missing; ignored by the
  target filter, included for realism). Existing live-device capacity (free 452 MB vs
  64 MB to absorb) passes `check_relocation_space`.
- `RemoveMissingPool::four_disk_two_missing()`: `pre_show =
  FOUR_DISK_TWO_MISSING_PRE_SHOW`, `usage_raw = usage_raw_four_disk_two_missing()`,
  `still_degraded_after: false`, `post_show` set to a coherent post-remove body (3 total:
  devid 1,2 present + devid 4 MISSING) for self-consistency, though it is never re-issued
  on the plan-gate-closed path.
- `PoolFixture::four_disk_devids_pinned()`: clone of `three_disk_devids_pinned`, looping
  over `(1,"disk1",1), (2,"disk2",2), (3,"disk3",3), (4,"disk4",4)`. Pinning devid 4 is
  conflict-free: `resolve_removal_target` only resolves the target (devid 3);
  `target_membership` retains devid 4 as a still-missing member post-remove (faithful to
  reality); nothing cross-checks membership size against `total_devices`. Devid 4's UUID
  seed is cosmetic (its mapper is never probed).

### E. `cli/src/replace.rs` -- mirror the plan-gate-closed end-to-end tests

Two tests, parallel to the remove-missing pair (B + C), so the unified gate has the
symmetric recovery-contract coverage the Decisions section commits to. Both model a pool
with two missing devices and replace one (the other stays missing -> still degraded ->
plan gate closes: `should_restore_raid1(will_clear_last_missing=false, ..)` at
`replace.rs:1740`, where `will_clear_last_missing` requires `pool.missing_count == 1`).

**E1 -- success path: `cmd_replace_missing_path_skips_soft_balance_when_not_last_missing`**,
parallel to the healthy-path
`cmd_replace_missing_path_runs_soft_balance_after_replace_and_resize` (`replace.rs:5522`).
Assert no `BtrfsBalanceRaid1Soft`, and **no `BtrfsFilesystemShow` after the
`BtrfsFilesystemResize`** (`pool_resize_device` at `replace.rs:925`). Do NOT assert "no
SHOW after `BtrfsReplaceStart`": `execute` issues an *unconditional* metadata-enrichment
probe (`probe_pool` -> `BtrfsFilesystemShow`) immediately after the replace
(`replace.rs:843`, feeding `enrich_from_pool_state`), so that earlier SHOW is expected and
would false-trip a post-replace assertion. The only SHOW *after the resize* is the
`maybe_restore_raid1` re-probe (gated at `replace.rs:928-936`), so "no SHOW after resize"
is the correct discriminator for the plan gate having closed. Use the `.position(...)`
idiom against `BtrfsFilesystemResize`.

**E2 -- required journal pin:
`cmd_replace_missing_path_not_last_missing_persists_restore_raid1_false`**. `execute`
rewrites the journal to `PostReplaceMaintenance` (`replace.rs:863-878`) *before* the resize
(`replace.rs:925`), so inject a `BtrfsFilesystemResize` failure -- the first fallible step
after the rewrite on the missing path (the old-mapper close at `replace.rs:891-923` is
live-only and skipped). Assert `cmd_replace` errors, then `journal::load_journal` returns
`OpKind::Replace { phase: ReplacePhase::PostReplaceMaintenance, source:
ReplaceJournalSource::Missing { .. }, restore_raid1_after_commit: false, .. }`. This pins
replace's persisted recovery flag so `braid recover` cannot replay an unowed soft balance
on the multi-missing missing path. Required, not optional -- symmetric recovery-contract
coverage is a stated decision.

Note: replace's test-fixture plumbing (`two_device_pool`, `replace_work_plan_test_pool`,
the missing-path command harness) is less scouted than remove-missing's; confirm the
multi-missing replace fixture shape (and that the resize-failure injection leaves the
journal in `PostReplaceMaintenance`) during implementation before finalizing assertions.

## Out of scope

- **`format_remove_missing_confirm` multi-missing branch** -- already unit-covered by
  `remove_missing_confirm_multiple_missing` (`remove_missing.rs:2502`) for the `(1,2)` and
  `(2,2)` cases. No end-to-end `yes=false` confirm-render test needed.
- **White-box plan-flag read** -- rejected in favor of the behavioral journal pin (C).

## Verification

1. `just test-rust` (or `cargo test -p braid-cli`). Confirm A/B/C and E1/E2 pass, and the
   existing `work_plan_steps_*` / `maybe_restore_raid1_*` / confirm tests still pass.
2. Run the two new remove-missing tests with `-- --nocapture` and eyeball the recorded
   command stream: the plan-gate test (B) shows exactly **one** `BtrfsFilesystemShow`
   (pre-remove, before `BtrfsDeviceRemove`); the relabeled runtime-veto test (A) shows
   **two** (one after the remove). This contrast is the discriminator.
3. Mutation check (manual, revert after): temporarily change `should_restore_raid1` to
   `clears_last_missing || present_after >= 2` (so it wrongly returns `true` for
   `missing_count >= 2`). Because the predicate is shared, expect: remove-missing B
   (post-remove re-probe now appears) and C (persisted flag now `true`) **fail**, replace
   E1 (post-resize re-probe now appears) and E2 (persisted flag now `true`) **fail**, the
   render tests `work_plan_steps_omit_rebalance_when_not_last_missing` and
   `dry_run_missing_not_last_omits_rebalance` also fail, and the relabeled test A still
   passes -- proving the new tests catch the regression the original cited test could not.
4. `cargo fmt` + `cargo clippy` clean; new `pub(crate)` fixtures carry `///` intent docs.

## Implementation notes

- Added the replace multi-missing topology in `cli/src/test_fixtures/replace.rs` so the replace end-to-end tests keep the still-missing disk4 row in target membership; the existing one-missing fixture could not pin the recovery journal artifact without dropping that member.
