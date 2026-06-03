# Skip the post-add RAID1 convert balance on a degraded pool

## Context

`braid add`, when expanding an *already-degraded* pool (pre-add
`missing_count > 0`), unconditionally runs a **hard** RAID1 convert balance
(`cli/src/add.rs#AddPlan::execute` -> `cli/src/pool.rs#pool_balance_raid1` ->
`btrfs balance start --enqueue -dconvert=raid1 -mconvert=raid1`) whenever the
post-add present-device count is `>= 2`. The present-device count excludes the
missing member (`probe.rs`: `missing_count = total_devices - devices.len()`),
so a 2-disk RAID1 with one member missing + one fresh disk always trips the
balance.

Today this hard balance apparently **succeeds**: it rewrites every chunk across
the two present devices and restores redundancy onto the new disk. (Inferred,
not yet directly observed: `tests/cli/braid-add-warnings.py` Phase 4 reuses the
Phase-3 pool and expects a `BraidLabeledNoBtrfs` error, *not* a pending-op
refusal -- which it would get if Phase 3's degraded add had stranded the
`PostAddBalanceRaid1` journal written at `add.rs:1518` and cleared only after a
successful balance. So Phase 3's add, balance included, must complete today. The
promoted Phase 3 VM test pins the new skip behavior empirically.) The change
therefore **defers** a currently-working restore -- it does not remove a no-op or
fix a guaranteed failure.

Deferring it is the better design for three reasons:

1. **A degraded convert is not the btrfs-recommended path for a missing device.**
   `reference/btrfs-progs/Documentation/btrfs-balance.rst` gives general guidance
   to "use `btrfs replace` or `btrfs device remove` to handle the failing/missing
   device first." (Its *acute* warning -- converting to **lower** redundancy with a
   **failing**, present-but-bad device -- is milder than our case: a convert *to*
   raid1 with a cleanly-**missing** device. We lean only on that closing
   "handle the missing device first" guidance, not the strong warning.) A full
   convert rewrites *all* data through the allocator while the pool has no
   redundancy -- a longer, less-targeted operation than the purpose-built
   `btrfs replace` braid's `replace` uses.
2. **braid's own documented workflow makes the add a precursor.**
   `docs/commands/remove-missing.md`: on a 2-disk degraded pool `remove-missing`
   refuses (can't drop RAID1 below two devices), so the operator runs `braid add`
   *then* `remove-missing`. Redundancy is restored at that repair step
   (`cli/src/pool.rs#maybe_restore_raid1`'s soft balance, plus
   `btrfs device remove missing` relocating data onto the new disk). Skipping at
   add establishes one predictable rule: *add never restores redundancy on a
   degraded pool; repair commands do.*
3. **It makes every degraded-add interrupt path converge.** After the change a
   degraded add runs no balance, so a completed add and *every* recover path end
   identically -- *device added, still degraded*, redundancy deferred to the
   repair step. Today that convergence has one gap: a completed add restores
   redundancy (hard balance), and a *forced-shutdown* interrupt also does --
   `cli/src/recover.rs#replay_owed_raid1_maintenance` checks for a paused balance
   and **resumes** it (`pool_balance_resume`, draining the persisted hard convert
   filters) *before* its soft replay, and a forced shutdown leaves the balance
   paused (`skip_balance` suppresses kernel auto-resume) -- but a rarer
   *umount-cancelled* interrupt falls through to the soft no-op and does not.
   Skipping closes that one divergent path.

`docs/commands/add.md` already tells the operator the pool "stays degraded" even
if the add succeeds; this change makes the code match that wording and pins it
with tests -- closing the gap the original finding raised
(`tests/cli/braid-add-warnings.py` Phase 3 explicitly punts on the downstream
outcome).

**Intended outcome:** a degraded `braid add` adds the disk and **skips** the
RAID1 convert balance, surfacing a single `[skip]` note (identical body in
dry-run preview stdout and real-run stderr); redundancy is restored by the
subsequent `remove-missing`/`replace`. Recover is unchanged.

## Design decision (the hard / soft / skip taxonomy)

This extends the existing distinction in `docs/internals/btrfs/balance-soft.md`
to three cases:

- **Hard convert** (`pool_balance_raid1`) -- healthy growth (3rd+ device).
  Rewrites every chunk to redistribute onto the new disk. **Skipped when
  degraded** -- redundancy restoration is deferred to the purpose-built repair
  path (see Context).
- **Soft convert** (`pool_balance_raid1_soft`) -- post-repair restoration
  (`remove-missing`, `replace`, recover replay). Only converts `single` chunks to
  `raid1`; **never rewrites existing `raid1` chunks**, so it never does a full
  degraded rewrite and is safe + beneficial even on a still-degraded pool.
  **Left running on degraded** -- gating it off would delete useful single-chunk
  cleanup.
- **Skip** -- degraded add (new). Surfaced as a single `PreviewNote::Skip`.

Rejected alternative (soft balance on the degraded add instead of skip): in the
common case (no writes since the disk went missing) soft has no `single` chunks
to convert, so it is a no-op that still prints a misleading "balancing to
RAID1..." line. It does real work only in the write-then-add case, where
deferring that single-chunk restoration to `remove-missing`/`replace` is exactly
the deliberate "add never restores redundancy on a degraded pool" invariant. So
soft is weaker than skip on both axes.

## Implementation

All code edits are in `cli/src/add.rs` (plus docs). Recover is untouched.

The execute path reads `self.pool.missing_count` directly (`self.pool` is a
`PoolState`). The preview path renders from `AddWorkPlan`, which carries
`existing_pool_device_count` but **no missing count** -- so the one structural
change is threading the missing count onto `AddWorkPlan`.

### Edit 1 -- thread the missing count onto `AddWorkPlan`

- Add `pre_add_missing_count: u64` to `struct AddWorkPlan` (~`add.rs:567`).
- Populate from `input.pool.missing_count` where `build_add_work_plan` returns
  the `AddWorkPlan` (~`add.rs:2185`); `input.pool: &PoolState` is in scope.
- Populate it in the `#[cfg(test)]` constructor `plan_for_execute_target`
  (~`add.rs:2934`) from the passed `pool.missing_count`, or existing execute
  tests fail to compile.

### Edit 2 -- preview Step gate (dry-run parity)

`AddWorkPlan::render`/step builder (~`add.rs:782-786`):

```rust
// before
let total_after = self.existing_pool_device_count + self.target_count();
if total_after >= 2 {
    steps.push(Step { /* btrfs balance to RAID1, BtrfsBalanceRaid1 */ });
}
// after
let total_after = self.existing_pool_device_count + self.target_count();
if total_after >= 2 && self.pre_add_missing_count == 0 {
    steps.push(Step { /* unchanged */ });
}
```

So the degraded preview omits the balance step. The `btrfs device add` step is
unchanged.

### Edit 3 -- a single `PreviewNote::Skip` note (one source, both modes)

The skip body must surface exactly once. `AddPlan::execute` already replays
**all** `self.notes` to stderr via `preview::emit_notes_to_stderr`
(`cli/src/add.rs:998`) *before* any mutation, so the explanation lives as a note
-- and the execute gate (Edit 4) must **not** also print it, or the body
double-emits on real runs.

`PreviewNote` (`cli/src/preview.rs:47`) has only `Info` / `Warn` / `PerDisk`; a
bare `Info` renders untagged. Add a top-level **`Skip(String)`** variant that
renders `[skip] <body>` -- mirroring how `Warn` renders `[warn] <body>` -- in
both render paths:
- `render_notes_for_stderr_with` (`preview.rs:183`) -- real-run stderr + the
  `cmd_add` Err path.
- `Preview::render` (`preview.rs:241`) -- dry-run stdout.

Rust match-exhaustiveness flags only the two exhaustive `match`es above (the
renderers); add a `[skip]`-rendering arm to each. Non-exhaustive sites need
nothing -- `if let` (including the `PerDisk` filter at `preview.rs:155`),
`filter_map(_ => None)`, and `other =>` catch-alls in `preflight.rs`,
`remove.rs`, `lock.rs`, `main.rs`, `remove_missing.rs`, `replace.rs`, and the
`add.rs` test note-extractors all ignore a `Skip` note (which only `plan_add`
produces). The `has_info_noop` "nothing to do" detection (`preview.rs:274`) keys
on `Info` only, so a `Skip` note does not interfere.

Add the shared body helper next to `format_add_missing_devices_warning`
(~`add.rs:883`):

```rust
/// Body for the degraded-add balance-skip note, rendered `[skip] <body>` in both
/// dry-run preview and real-run stderr via `PreviewNote::Skip`. One source so the
/// two modes never drift (mirrors `format_add_missing_devices_warning`).
fn format_add_degraded_balance_skip() -> String {
    "pool: RAID1 balance skipped -- pool still has a missing device; redundancy \
     not restored. Run `braid remove-missing` or `braid replace` to restore it.".into()
}
```

Push exactly **one** `PreviewNote::Skip(format_add_degraded_balance_skip())` in
`plan_add` *after* `build_add_work_plan` (alongside the keyfile-asymmetry block,
~`add.rs:1796`-1825, where `work_plan` exists -- the earlier missing-warn push at
1765 is before the plan is built and cannot see these fields), gated on the exact
balance-step condition plus no-op/mount guards:

```rust
if !work_plan.is_noop()
    && work_plan.pool_was_mounted
    && (work_plan.existing_pool_device_count + work_plan.target_count()) >= 2
    && work_plan.pre_add_missing_count > 0
{
    notes.push(PreviewNote::Skip(format_add_degraded_balance_skip()));
}
```

The `!is_noop()` guard stops the note mis-firing next to the "nothing to do" Info
note on a no-op re-add of an already-present disk on a 3+-device degraded pool
(`target_count() == 0` but `existing + 0 >= 2`).

### Edit 4 -- execute gate only (no second message)

`AddPlan::execute` live-pool branch (~`add.rs:1521-1545`):

```rust
let total_after = self.pool.devices.len() + mapper_paths.len();
if total_after >= 2 && self.pool.missing_count == 0 {
    // unchanged: [wait] "pool: balancing to RAID1..." -> pool_balance_raid1 -> [ok]
}
// degraded (missing_count > 0): no balance and NO eprint here -- the
// PreviewNote::Skip from Edit 3 already emitted at add.rs:998.
```

No `else` arm. A second `[skip]` eprint here would double the body on real runs
(the note is replayed at `add.rs:998` before mutation). **Leave the
`PostAddBalanceRaid1` journal write (`add.rs:1518`) and `clear_journal`
unchanged** -- safe because the only mutation a degraded add performs is
`btrfs device add` (PoolMutation phase); the `PostAddBalanceRaid1` window now
does no balance, so an interrupted degraded add recovers via recover's soft
no-op to the *same* end state (device added, still degraded), keeping add and
recover consistent without a recover edit.

### Recover -- unchanged (deliberate)

`replay_owed_raid1_maintenance` keeps running its soft balance on `>= 2` present
devices. On a degraded pool that is a safe, beneficial single-chunk cleanup (see
taxonomy). Gating it on `missing_count == 0` would be a mild regression, not a
symmetry win. Document the rationale in `balance-soft.md` rather than changing
code.

## Tests

### Unit (`cli/src/add.rs`, structure-insensitive request assertions)

- **`execute_degraded_add_skips_raid1_balance`** -- mirror
  `execute_fresh_add_to_mounted_single_device_pool_balances_once` (~`add.rs:4588`)
  but build the planned `PoolState` with `missing_count: 1, total_devices: 2`,
  one present `PoolDevice` (disk1). Assert: `result.is_ok()`, **zero**
  `CmdRequest::BtrfsBalanceRaid1`, and `BtrfsDeviceAdd` **is** issued.
  - `validate_execute_pool_identity` (`add.rs:294`) only compares `mounted` +
    `fsid`, so the gate fires off the planned `self.pool.missing_count` without
    forcing the fresh-probe mock to match. **Preferred for fidelity:** add a
    small `with_missing`-style flag to `RecoverableAddRunner::pool_show`
    (~`add.rs:4276`) emitting a `path MISSING` devid row + bumped `Total devices`,
    so the fresh probe is also degraded and the test models reality. Mirrors the
    existing missing-row synthesis in `AddPlanTestRunner` (~`add.rs:8369`).
- **`plan_add_degraded_preview_omits_balance_step`** -- use
  `AddPlanTestRunner::new().with_missing(1)` (probe path already wired) + a fresh
  target, then `plan.preview().render()`. Assert the render contains
  `btrfs device add`, does **not** contain `btrfs balance to RAID1`, and contains
  the `[skip] pool: RAID1 balance skipped` note body **exactly once**. Sibling to
  `plan_add_render_emits_warn_above_steps` (~`add.rs:9329`).

Confirm green (no edits): `execute_fresh_add_..._balances_once` (healthy,
`missing_count:0`), `dry_run_render_add_to_existing_pool_with_balance`
(~`add.rs:7280`, healthy), `plan_add_render_emits_warn_above_steps` (asserts the
device-add step, not the balance).

### VM

- **Extend `tests/cli/braid-add-warnings.py` Phase 3 in place.** The fixture is
  already degraded (disk2 missing). Promote `machine.execute` -> `machine.succeed`
  (the add should now reliably succeed -- this *empirically confirms*
  `btrfs device add` works on a degraded mount, which the old test ducked).
  Add assertions: exit 0; the `[skip] pool: RAID1 balance skipped ...` body on
  stderr appears **exactly once** (guards against the Edit-3/Edit-4 double-emit
  regression); **negative** guards that the `[wait]/[ok]` "balancing to RAID1"
  lines do **not** appear; `btrfs fi show` lists `braid-disk3` and still shows
  `missing`; profile preserved. Update the Phase 3 preamble (Intent/Why/Scenario)
  to state it now pins the skip, not just the warning wiring.
  `braid-add-warnings.nix` is already registered in `flake.nix` -- no flake edit.

- **New E2E `tests/cli/add-degraded-then-remove-missing.{nix,py}`** -- pins the
  *safety rationale*: redundancy is restored at the repair step, which justifies
  skipping it at add. Sequence: build 2-disk RAID1 -> kill disk2 (unmount, close
  mapper, mount `-o degraded`) -> `braid add disk3` (assert `[skip]`, still
  `missing`, 3 devids, `Data, RAID1` profile preserved, disk3 present-but-empty)
  -> `braid remove-missing --missing-id <devid> --yes` -> assert **no
  `missing`**, `Data, RAID1` across disk1+disk3, 2 devids. Fixture: 3x 1024 MiB
  disks; model on `replace-dead-disk.{nix,py}` for the degraded-synthesis +
  `btrfs fi df`/`fi show` assertions. **`remove-missing` requires `--missing-id`**
  -- there is no auto-detect (`RemoveMissingArgs.missing_id: u64`, `main.rs:328`);
  invoke `braid remove-missing --missing-id 2 --yes` (devid 2 is the killed
  disk2 in this fixture; parse the `missing` devid from `btrfs fi show` if you
  prefer not to hard-code it). **Register in `flake.nix`** (explicit entry
  `add-degraded-then-remove-missing = pkgs.testers.nixosTest (import ./tests/cli/add-degraded-then-remove-missing.nix { braid = linuxCrane.braid; });`
  -- no auto-discovery). Add the standard Intent/Why/Scenario preamble.

## Docs

- `docs/commands/add.md`: step 6 of "What happens under the hood" (~line 92) --
  add the degraded carve-out ("...balances data to RAID1, **unless the pool has a
  missing device, in which case the balance is skipped** -- redundancy is restored
  by a later `remove-missing`/`replace`"). Tighten the safety-checks
  missing-device bullet (~line 115) to mention the skip and the
  `add`-then-`remove-missing` path (already documented in `remove-missing.md`).
- `docs/internals/btrfs/balance-soft.md`: add the **degraded-add skip** case to the
  hard-vs-soft section. Frame it as a *deferral*, not a hazard fix: the current
  hard convert succeeds but rewrites *all* data while the pool has no redundancy,
  so braid defers redundancy restoration to the purpose-built
  `replace`/`remove-missing`; contrast the soft balance, which only converts
  `single`->`raid1` and is safe on degraded. State the convergence accurately and
  consistently with this doc's existing Recover-replay section (which resumes a
  paused balance before the soft replay): with the hard balance skipped, a
  completed degraded add and every recover path end at "device added, still
  degraded" -- whereas *today* a forced-shutdown interrupt still restores
  redundancy by resuming the paused hard balance, and only a umount-cancelled
  interrupt diverges via the soft no-op. Cite the existing `btrfs-balance.rst`
  "handle the missing device first" line (already in Sources) as general
  guidance, not a strong prohibition.
- No ADR change required (ADR-001/022 are referenced from code comments, not
  contradicted). `add.md` PostAddBalanceRaid1 wording (~line 127) stays accurate
  (recover's owed balance is the soft no-op on degraded).

## Verification

- `just test-rust` -- new + existing unit tests (CLI package is `braid-cli`).
- `just test-vm braid-add-warnings add-degraded-then-remove-missing` -- the two
  VM tests (focused; the full suite is 20-30 min). This is the empirical check
  that `btrfs device add` succeeds on a degraded mount and that `remove-missing`
  restores RAID1 across the two present devices on the pinned kernel.
- `mdbook build docs` -- validates `docs/` cross-links (`mdbook-linkcheck2`).
- Blast radius is localized to `add` planning/execute + preview + docs + two VM
  tests; no parser/tool-version change, so **no fixture refresh**. Per AGENTS.md,
  hand back to the user for a full-suite run before merge rather than running the
  unscoped suite autonomously.

## Out of scope

- Recover behavior (deliberately unchanged; see rationale above).
- Refusing degraded adds outright -- rejected; `add`-then-`remove-missing` is a
  documented supported workflow.
- The healthy 3rd+-device hard balance -- unchanged.

## Implementation notes

- Execute-test fidelity: took the plan's "preferred for fidelity" option --
  added `RecoverableAddRunner::degraded()`, which makes `pool_show` synthesize a
  `path MISSING` row and bump `Total devices` so the fresh execute-time probe is
  degraded too, rather than relying only on the planned `PoolState.missing_count`.
- New E2E fixture disk size: bumped from the plan's 3x 1024 MiB to 3x 4096 MiB.
  Because the degraded `add` leaves disk3 empty (balance skipped), `remove-missing`
  must relocate a full copy onto it; braid's survivor-capacity preflight
  (`raid1_chunk_pair_capacity`) bottlenecks on disk1 -- the surviving copy holder --
  whose unallocated headroom on 1024 MiB disks (~352 MiB) fell below the missing
  device's fixed ~416 MiB baseline chunk allocation, so `remove-missing` was
  refused ("not enough space to relocate Data chunks"). The ~416 MiB is a fixed
  mkfs/convert baseline (the written file is 15 bytes), so it does not scale with
  disk size; 4096 MiB gives disk1 ample headroom. Added `btrfs device usage` /
  `btrfs fi df` diagnostic prints before the `remove-missing` so any future sizing
  regression is self-explanatory in the VM log.
- Fixed the now-stale Scenario comment in `plan_add_render_emits_warn_above_steps`
  ("... `btrfs device add` + balance steps" -> device-add only): its `with_missing(1)`
  pool now skips the balance. Test logic is unchanged; the plan listed it under
  "confirm green (no edits)", but the comment had become factually wrong.
- Broadened the `braid-add-warnings.nix` top-of-file `What:` comment to mention the
  skip pin (the plan scoped the edit to the `.py` Phase 3 preamble); kept minimal.
- New E2E parses the missing devid via `braid status --json` (the plan allowed
  hard-coding `--missing-id 2` or parsing), mirroring the sibling
  `add-returned-disk-after-remove-missing.py`.

## Follow Up

- `tests/cli/braid-add-warnings.nix` `Why:` paragraph still claims the real-run
  "must preserve today's `warning: pool has ...` stderr wording byte-identically
  so log scrapers do not drift" -- pre-existing drift from the PR-7 `[warn]`
  migration (the `.py` now asserts `warning:` is gone and `[warn]` is used). Left
  untouched as out of scope for this change; worth a follow pass to realign that
  paragraph with the `[warn]` contract.
