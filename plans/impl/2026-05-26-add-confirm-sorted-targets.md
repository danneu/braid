# Pivot: derive add `confirm_disks` from work targets

## Context

`braid add disk1=... disk2=...`, where `disk1` is already a live pool member
and `disk2` is fresh, prints a confirmation that lists **both** disks under
`Add to pool:` even though `disk1` produces no work. The over-listing is real
and also runs a redundant `query_disk_hw_info` (lsblk) for the already-in-pool
disk.

Root cause: `build_add_credential_prelude` (`cli/src/add.rs:1811`) builds
`confirm_disks` by zipping the full `input.names/by_ids/probed`, while
`build_add_work_plan` (`cli/src/add.rs:1926`) skips already-in-pool disks via
`continue` (lines 2001, 2066) so they never become `targets`. The two sets
diverge.

This mirrors a fix already applied to the *verify* side: commit `e56a74a`
("skip duplicate credential checks for live members") excludes already-in-pool
disks from `verify_targets` by LUKS UUID. The confirmation prompt is the
un-fixed sibling.

**Outcome:** make `confirm_disks` a function of the actual work `targets`, so the
"Add to pool:" list can never name a disk that won't be added -- divergence
becomes structurally impossible rather than something a filter must remember to
do. Add a `plan_add`-level regression test that pins the mixed case.

Every non-erroring branch of `build_add_work_plan` that produces no target is an
already-in-pool `SameBacking` `continue`; every other present disk
(`PresentNotLuks` -> `Fresh`, `PresentLuks`/`NoMatch` -> `OpenRecoverable` /
`ClosedPresentLuks`) becomes a target. So deriving `confirm_disks` from `targets`
excludes exactly the already-in-pool disks and nothing else.

The derived list is iterated through the **same `DiskName`-sorted view** as the
rest of the operator-visible add workflow. `cli/src/add.rs:561-567` documents the
invariant that every operator-visible iteration of work-plan targets MUST use
`targets_sorted_by_name()` (fresh targets get random per-disk UUIDs, so any
UUID-keyed order is effectively random per invocation). Once the confirmation
prompt is target-derived it becomes exactly such an iteration, so it must sort --
this also dissolves the existing prompt-vs-dry-run ordering mismatch instead of
preserving it.

## Changes (`cli/src/add.rs`)

### 1. Add a `by_id()` accessor to `AddTargetWork`

The enum already has `mapper_path()` (510) and `name()` (521) but no uniform
`by_id` accessor. Add one mirroring `name()` (every variant's inner struct
carries `by_id: ByIdPath`), with a one-line `///` in the same style as `name()`:

```rust
/// Borrow the target's hardware `by_id`. Used to build the add
/// confirmation list from the actual work targets so the prompt never
/// names a disk that won't be added.
fn by_id(&self) -> &ByIdPath {
    match self {
        AddTargetWork::Fresh(target) => &target.by_id,
        AddTargetWork::OpenRecoverable(target) => &target.by_id,
        AddTargetWork::ClosedPresentLuks(target) => &target.by_id,
    }
}
```

### 2. Share the `DiskName`-sort as one reusable view

`AddWorkPlan::targets_sorted_by_name` (`cli/src/add.rs:561-572`) is the canonical
operator-visible order, but it is a `&self` method -- and the prelude is built
*before* the `AddWorkPlan` exists (it is a field in the struct literal at line
2099-2100). To let `build_add_credential_prelude` reuse the identical order
without copying the comparator, extract the sort into a module-level helper over
a borrowed slice and have the method delegate:

```rust
/// Single definition of the operator-visible add-target order (sorted by
/// `DiskName`). The confirmation prelude is built before the `AddWorkPlan`
/// exists, so it cannot call the `&self` method; both go through this so the
/// prompt, dry-run steps, and execution progress lines share one order.
fn sort_targets_by_name(targets: &[AddTargetWork]) -> Vec<&AddTargetWork> {
    let mut v: Vec<&AddTargetWork> = targets.iter().collect();
    v.sort_by(|a, b| a.name().cmp(b.name()));
    v
}
```

Keep the existing invariant doc on `AddWorkPlan::targets_sorted_by_name` (it is
the canonical statement of the MUST-sort rule) and have its body delegate:
`sort_targets_by_name(&self.targets)`. One sort definition, so the prompt order
and the step order cannot drift apart.

### 3. Derive `confirm_disks` from the sorted targets view in `build_add_credential_prelude`

Change the signature to take the built work targets and build `confirm_disks`
from the sorted view instead of from `input.names/by_ids/probed`:

```rust
fn build_add_credential_prelude(
    input: &AddStepsInput<'_>,
    targets: &[AddTargetWork],
) -> AddCredentialPrelude {
    let confirm_disks = sort_targets_by_name(targets)
        .into_iter()
        .map(|t| AddConfirmDiskPlan {
            name: t.name().clone(),
            by_id: t.by_id().clone(),
            needs_luks_format: matches!(t, AddTargetWork::Fresh(_)),
        })
        .collect();
    // ... confirm_new / verify_targets / pool_target_count unchanged ...
}
```

- `needs_luks_format` becomes `matches!(t, AddTargetWork::Fresh(_))` -- `Fresh` is
  the only variant that gets LUKS-formatted (`OpenRecoverable` /
  `ClosedPresentLuks` are existing-LUKS disks), equivalent to the old
  `PresentNotLuks` check.
- **Leave `confirm_new`, `verify_targets`, `pool_target_count` as-is.** They
  read `input` and are unaffected: an already-in-pool disk is always
  `PresentLuks`, never `PresentNotLuks`, so it never contributed to
  `any_needs_format`/`confirm_new`, and `verify_targets` is a deliberately
  different set (all live members + non-pool `PresentLuks` candidates).
- **Ordering:** name-sorted via the shared helper, so the prompt matches
  `render_steps()` and execution progress order and upholds the
  `targets_sorted_by_name` invariant (`cli/src/add.rs:561`).
- Add a short `///` to the function capturing the invariant: *confirm_disks
  mirrors the (sorted) work targets so the prompt never lists a disk that won't
  be added and never reorders relative to the dry-run steps.*

### 4. Update the single call site

`cli/src/add.rs:2100` (inside `build_add_work_plan`, where the local `targets`
vector already exists):

```rust
prelude: build_add_credential_prelude(input, &targets),
```

This is the only production caller. The hand-built `AddCredentialPrelude` in the
test fixture at `cli/src/add.rs:2854` does not call this function, so it is
unaffected.

## Test (`cli/src/add.rs`, in `mod tests`)

Add one `plan_add`-level test next to the other `plan_add` boundary tests
(near `plan_add_already_in_pool_is_note_only_success`, ~8277). No existing test
covers this: the mixed test at `7696` runs `cmd_add` with `yes: true` (so the
prompt is bypassed) and aborts at a forced header-backup failure, asserting only
`verify_devices`; the `format_add_confirm` unit tests (2297-2370) feed
hand-built lists. `confirm_disks` is only consumed by the interactive prompt
(execute, 947-962) and is not rendered in dry-run, so the field is the correct
test surface.

Reuse the proven mixed-scenario fixtures (`disk1` = already-in-pool live LUKS
member, `disk2` = fresh) exactly as the `7696` test constructs them:
`add_test_setup()`, `AddMockFs` with both `virtio-disk{1,2}` by-ids,
`AddRecordingRunner::new(true).with_disk1_present_luks_member()`, and
`mock_virtio_offset_backing_path_resolver()`. Call `plan_add` directly (not
`cmd_add`) so it returns an `AddPlan` without execution -- `plan_add` issues a
strict subset of what `cmd_add` ran in `7696`, so the runner already mocks
everything planning needs, and the forced header-backup failure never fires.

Assert on the derived list (exact-vector equality pins both "disk2 present" and
"disk1 absent"; only the fresh disk survives, so the name-sort from change 3 does
not alter this single-element result):

```rust
let plan = plan_add(&runner, &fs, &AddParams { /* disk1=..., disk2=...; dry_run:true; yes:true; passphrase_file: Some(pass_path) */ })
    .expect("plan_add should succeed for mixed already-in-pool + fresh add");

let confirm_names: Vec<&str> = plan
    .work_plan
    .prelude
    .confirm_disks
    .iter()
    .map(|d| d.name.as_str())
    .collect();
assert_eq!(
    confirm_names,
    vec!["disk2"],
    "confirmation must list only the fresh disk, not the already-in-pool disk1"
);
assert_eq!(plan.work_plan.targets.len(), 1, "only the fresh disk is real work");
assert!(
    plan.work_plan.prelude.confirm_disks[0].needs_luks_format,
    "the fresh disk must be flagged for LUKS format"
);
```

All accessed fields (`work_plan`, `prelude`, `confirm_disks`, `name`,
`needs_luks_format`) are private but in-module, so they are reachable from
`mod tests`.

Include the required `//` preamble (Test Conventions): **Intent** -- a mixed add
lists only the fresh disk in the confirmation; **Why it exists** -- guards the
`confirm_disks`-from-targets invariant against regressing back to over-listing
already-in-pool disks; **Scenario** -- operator re-passes an in-pool disk
alongside a new one and the "Add to pool:" prompt must not imply the in-pool
disk is being re-added.

## Non-goals

- Do **not** touch `verify_targets` -- it is correct and intentionally a
  different set.
- Do **not** add a second test that pins prompt *ordering*. Order is now
  structural: the prompt and `render_steps()` iterate the single
  `sort_targets_by_name` helper (change 2), so they cannot drift, and the
  mixed-case test below leaves only one target so it cannot demonstrate sort
  order anyway. A dedicated ordering test would only re-cover the shared helper.

## Verification

- `just test-rust` -- runs `cargo test` for `braid-cli`. Confirm the new test
  passes and that the existing `cmd_add_mixed_already_in_pool_and_fresh_verifies_each_disk_once`,
  `plan_add_already_in_pool_is_note_only_success`, and the three
  `add_confirm_*` / `format_add_confirm` tests still pass (none depend on the
  changed signature). The `targets_sorted_by_name` delegation (change 2) returns
  the same order it does today, so any existing `render_steps` ordering tests are
  unaffected.
- Optional sanity check that the test fails before the fix: temporarily revert
  the change-3 derivation and confirm the new test fails with
  `confirm_names == ["disk1", "disk2"]` (disk1 sorts before disk2), proving it
  pins the regression.

## Files touched

- `cli/src/add.rs` -- `by_id()` accessor (~509-528); `sort_targets_by_name` helper
  extracted with `AddWorkPlan::targets_sorted_by_name` delegating to it
  (~561-572); `build_add_credential_prelude` signature + sorted `confirm_disks`
  derivation (~1811); call site (2100); and one new test in `mod tests`.

## Implementation notes

- The current tree already had `confirm_disks` deriving from work targets and
  the done summary deriving from target names via `e53f5de`; this implementation
  kept those surfaces and completed the remaining shared sorted helper plus the
  `plan_add` regression.
