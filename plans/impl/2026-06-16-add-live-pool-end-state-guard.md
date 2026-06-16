# Pivot: keep the live-pool add end-state guard, but make it self-justifying, consistent, and tested

## Context

A Low/Simplicity review finding proposed deleting the trailing `journal_targets`
verification sweep in `AddPlan::execute` (`cli/src/add.rs`), claiming it merely
re-checks what the per-target `find_added_device_by_uuid` loop already proved, with
"behavior preserved."

Investigation showed the headline claim is **wrong for multi-disk adds**, but the
finding surfaces real warts worth fixing:

- The per-target loop checks each target against its **own** post-add probe
  (`probe_i`, fresh each iteration) and extracts `dev.devid` for
  `alert::drop_ghost_acked_for_devids`. That presence check is a byproduct of
  needing the devid -- it is not removable.
- The trailing sweep checks **all** targets against `pool_after` (= the **final**
  probe `probe_N`). For a single-disk add these coincide (pure redundancy); for a
  multi-disk add -- a first-class supported path, see
  `cmd_add_partial_multi_add_journal_carries_all_targets` -- they diverge. The sweep
  uniquely catches a device that was added successfully (present in its own probe)
  but **vanished from the pool before the final probe** (a disk failing mid-multi-add).
- `membership::enrich_from_pool_state` iterates `pool.devices` and **silently skips**
  any member absent from the pool (it never errors). So without the sweep, a vanished
  member would be persisted to `pool.json` with `devid: None`, after which
  `pool_balance_raid1` runs (`missing_count` is the plan-time value, still 0) against a
  now-degraded pool -- a messy failure instead of a clean fail-closed stop. The sweep
  is therefore a genuine fail-closed guard, not redundancy.
- The two checks emit **different error variants for the same lifecycle failure**: the
  per-target check returns `AddError::PostAddProbeFailed` (Display points the operator
  to `braid recover`), while the sweep returns a bare `AddError::Validation(String)`
  with **no remediation** -- even though both fail after `btrfs device add` committed
  but before `save_membership` writes `pool.json`, with the journal still in the
  `PoolMutation` phase (identical recovery semantics). The sweep gives strictly worse
  guidance for an identically-recoverable situation.
- The sweep was added deliberately (`982bc6f8 fix(add): make existing-pool add
  recovery phased`), but its distinct purpose is undocumented and its unique path is
  untested -- the only related test, `cmd_add_post_add_probe_uncertainty_is_fatal`, is
  single-disk, where the sweep is fully redundant with the per-target check.

**Intended outcome:** keep the guard, reconcile its error variant with the per-target
check, document why it exists distinctly, and lock its unique behavior with a
regression test. After this, the code no longer reads as an unexplained near-duplicate
(so the same finding is not re-filed), and the rare mid-multi-add disk-disappearance
fails closed with correct, recover-pointing guidance.

## Decision

Do **not** drop the sweep. Three changes:

1. **Reconcile the error variant.** Have the sweep emit `AddError::PostAddProbeFailed`
   instead of bare `AddError::Validation`. Justified by `safety-heuristics.md`:
   "Split post-commit failure variants by the operator's remediation and on-disk
   consequence, not by implementation layer." The sweep's remediation (`braid recover`)
   and on-disk consequence (device added in btrfs, `pool.json` unwritten, journal
   pending) are identical to `PostAddProbeFailed`, so it is the same variant -- not a
   new one. The `detail` string distinguishes the cause.

2. **Document the distinct purpose** with a comment at the sweep, so a future reader
   does not re-file this finding.

3. **Add a multi-disk regression test** that exercises the sweep's unique path (an
   earlier-added device vanishing before the final probe), which requires a small,
   localized test-harness extension.

## Changes

### 1. Reconcile the error variant -- `cli/src/add.rs`, `AddPlan::execute` (live-pool `else` branch)

Replace the trailing sweep's `AddError::Validation(...)` with `PostAddProbeFailed`:

```rust
for (uuid, target) in journal_targets.iter() {
    if find_added_device_by_uuid(&pool_after, uuid).is_none() {
        return Err(AddError::PostAddProbeFailed {
            detail: format!(
                "{}: no longer present in the live pool after all disks were added",
                target.name
            ),
        });
    }
}
```

Keep `pool_after` (still consumed by `enrich_from_pool_state` immediately below) and
keep the loop over `journal_targets` (it correctly covers `ClosedPresentLuks`-SamePool
targets, which land in `journal_targets` at runtime as well as in `needs_pool_add`).

No change to the `AddError` enum -- `PostAddProbeFailed` already exists with the right
Display/remediation (`cli/src/add.rs`, `AddError::PostAddProbeFailed`).

### 2. Explanatory comment -- replace the existing `// Reuse the last per-target probe...` line

State the sweep's distinct role: it is the **end-state post-condition** over every
journaled member against the final pool probe, distinct from the per-target loop (whose
job is to extract each devid for ghost-acked cleanup and prove each device live in its
*own* post-add probe). Note *why* it must fail closed here: `enrich_from_pool_state`
silently skips members absent from the pool, so a device that vanished mid-multi-add
would otherwise be persisted to `pool.json` (`devid: None`) and then balanced into a
degraded pool. Note *why* it shares `PostAddProbeFailed`: same lifecycle point and
remediation per `docs/dev/safety-heuristics.md` (split variants by remediation, not by
detection site; fail closed before a persistent/destructive op). ASCII only per repo
convention.

### 3. Test-harness extension -- `AddFullPathRunner` (`cli/src/add.rs`, `#[cfg(test)] mod tests`)

The existing `with_new_mapper_omitted_from_probe` omits **all** added mappers
unconditionally, which trips the per-target check first and never reaches the sweep. To
hit the sweep, disk2 must be present in its **own** post-add probe but absent from the
**final** probe. Add a conditional omission:

- New field on the struct (default `None`): `vanished_after_later_add: Option<String>`.
- New builder: `fn with_mapper_vanished_after_later_add(mut self, mapper: &str) -> Self`.
- In `AddFullPathRunner::pool_show`, after the existing `added` extension, drop the
  named mapper **only once it is no longer the most-recently-added** entry:

```rust
if let Some(vanished) = &self.vanished_after_later_add {
    let added = self.added.lock().unwrap();
    if added.last().map(|m| m != vanished).unwrap_or(false) {
        mappers.retain(|m| m != vanished);
    }
}
```

This reproduces the scenario exactly: probe after disk2's add (`added == [braid-disk2]`)
still lists disk2 (per-target check passes); probe after disk3's add
(`added == [braid-disk2, braid-disk3]`) drops disk2 (per-target check for disk3 passes;
sweep catches disk2). The harness already maps `braid-disk3` to devid 3 / `/dev/vdd` /
a distinct LUKS UUID (`mapper_devid`, `mapper_underlying`, `luks_uuid_for_device`), and
records each freshly-formatted UUID in `formatted_uuids`, so both rows probe and resolve
correctly.

### 4. Regression test -- `cli/src/add.rs`, `#[cfg(test)] mod tests`

Add `cmd_add_earlier_disk_vanishing_before_final_probe_is_fatal`, modeled on
`cmd_add_post_add_probe_uncertainty_is_fatal` and the multi-disk
`cmd_add_partial_multi_add_*` tests. Open the test with the required
Intent/Why-it-exists/Scenario preamble (see `docs/dev/testing.md`).

- Setup via `add_test_setup()` (disk1 pre-seeded; UUID seed 502).
- `runner = AddFullPathRunner::live().with_mapper_vanished_after_later_add("braid-disk2")`.
- `disk_specs: &["disk2=/dev/disk/by-id/virtio-disk2", "disk3=/dev/disk/by-id/virtio-disk3"]`,
  `fs` listing both by-id paths, `RecordingInhibitor`/`RecordingConfirm`,
  `mock_virtio_backing_path_resolver()`.
- Assert:
  - `Err(AddError::PostAddProbeFailed { .. })` -- this is the assertion that fails
    pre-pivot (currently `Validation`), proving both that the sweep is reached and that
    the variant was wrong.
  - rendered error (`err.to_string()`) contains `braid recover` -- pins the remediation
    (structure-insensitive).
  - `runner.added_mappers() == ["braid-disk2", "braid-disk3"]` -- both adds committed
    before the sweep caught the disappearance.
  - `journal::load_journal(&paths).unwrap().is_some()` -- journal survives for replay.
  - `pool.json` still lists only disk1: `load_membership(&paths)` has
    `by_name(disk2).is_none()` and `by_name(disk3).is_none()`.

## TDD order

Per `AGENTS.md`: add the harness builder + the new test first; run it and confirm it
fails for the right reason (`expected PostAddProbeFailed, got Validation(...)`), which
proves the sweep is the code under test. Then apply change (1) and confirm it passes.
Add comment (2) last.

## Out of scope (noted, not done here)

`replace.rs` post-mutation errors also use a bare `ReplaceError::Validation` with no
`braid recover` remediation, and `replace` has no trailing sweep at all (it persists
from a single `pool_after`). That is a separate command with a different shape; aligning
its post-commit messaging is a distinct follow-up, not part of this finding's root cause.

## Verification

- `just test-rust` (the canonical Rust lane), or for focused iteration
  `cargo test --lib cmd_add_earlier_disk_vanishing_before_final_probe_is_fatal` -- the
  unit tests compile into the `braid-cli` lib, and the `justfile` explicitly steers away
  from `cargo test -p <name>`. The new test passes; existing `cmd_add_*` tests stay green.
- `cargo clippy` clean.
- `scripts/docs/check-output-ascii.py` (or `just`'s lint lane) -- the new comment and
  `detail` string are ASCII only.
- Confirm no doc references the old message: `grep -rn "was not found in the live pool
  after add" docs/ cli/` returns nothing after the edit (it appears only at the code
  site today), so no doc update is required.

## Critical files

- `cli/src/add.rs` -- `AddPlan::execute` (sweep variant + comment), `AddFullPathRunner`
  (`pool_show` + new field/builder), new test in `#[cfg(test)] mod tests`.
- Reused as-is: `AddError::PostAddProbeFailed`, `find_added_device_by_uuid`,
  `membership::enrich_from_pool_state`, `add_test_setup`,
  `crate::test_fixtures::mock_virtio_backing_path_resolver`.
- Authority cited in the comment: `docs/dev/safety-heuristics.md`.
