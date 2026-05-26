# Fix: `braid add` names already-in-pool no-op disks in its confirm prompt and done summary

## Context

Two operator-facing surfaces of `braid add` can name disks it does not touch.
For a mixed invocation like `braid add disk1=... disk2=...` where `disk1` is
already a live pool member (classified as a `SameBacking` no-op and skipped)
and `disk2` is fresh:

- The pre-mutation confirmation prompt still lists `disk1` under "Add to pool:".
- The post-mutation success summary still prints `Done. disk1, disk2 are now
  part of the pool.` -- claiming this command added `disk1` when it skipped it.

The confirmation screen is the operator's last sanity check before destructive
work; the done summary is the operator's record of what the command did. Both
overstate the work and can mislead an operator into believing a no-op disk was
re-touched. Neither corrupts state (the work plan correctly skips `disk1`), but
both are operator-facing-integrity defects.

### Root cause (verified)

This is a partial-migration regression. `build_add_credential_prelude`
(`cli/src/add.rs:1811`) builds `confirm_disks` by zipping the *raw probed list*
(`names`/`by_ids`/`probed`), so it includes every disk the operator named.
Meanwhile `build_add_work_plan` (`cli/src/add.rs:1926`) classifies each disk
and skips `SameBacking` no-ops via `continue` (`add.rs:2001`, `:2066`), pushing
only real work into `targets`.

The semantic work-plan refactor (`b212a5d`) migrated the **dry-run preview** to
iterate `targets` (`render_steps` -> `targets_sorted_by_name`, `add.rs:574`)
but left the **interactive confirm prompt** building `confirm_disks` from the
probed list. The two operator-facing surfaces now disagree.

The success summary has the same root cause: `AddPlan::execute` calls
`format_add_done(&self.names)` (`add.rs:1471`), where `self.names`
(`add.rs:899`) is the full parsed input-name list (`add.rs:1523`), not the
work-plan target set. Both defects are the same class -- operator-facing output
derived from the raw input list instead of the post-classification `targets`.

### Supporting evidence

- **Sibling commands are already correct.** `remove.rs:254/257` and
  `replace.rs:449-455` build their confirm structs directly from the work plan
  (`work_plan.target_underlying`, `work_plan.name`). `add` is the outlier.
- **Documented design intent matches the fix.** The unified-confirmation-prompts
  plan (`plans/impl/2026-04-03-unified-confirmation-prompts.md:56`) states the
  prompt should confirm disks because "adding to the pool changes topology even
  for already-LUKS disks." A `SameBacking` no-op changes no topology, so
  excluding it realizes the intent precisely. Building from the probed list was
  a proxy that over-counts once no-op skips were introduced.
- **No test pins the buggy behavior.** The matching scenario test
  `cmd_add_mixed_already_in_pool_and_fresh_verifies_each_disk_once`
  (`add.rs:7696`) runs with `yes: true`, skipping the confirm block entirely;
  it only asserts credential-verify counts. Nothing covers `confirm_disks`
  filtering, and no VM/snapshot/golden test captures the "Add to pool:" text
  (only plan files mention it).

## The fix

Make `AddWorkPlan.targets` the single source of truth for every operator-facing
post-classification disk-name list -- both the confirmation prompt
(`confirm_disks`) and the success summary (`format_add_done`) -- so each names
exactly the disks that will be formatted/opened/added. All edits are in
`cli/src/add.rs`.

### 1. Add a `by_id()` accessor to `AddTargetWork`

`AddTargetWork` (`add.rs:509`) already has `mapper_path()` and `name()` but no
`by_id()`. All three variants (`Fresh`, `OpenRecoverable`, `ClosedPresentLuks`)
carry a `by_id: ByIdPath` field. Add an accessor mirroring `name()`:

```rust
/// Borrow the target's hardware `ByIdPath` so the confirmation prelude
/// names exactly the disks that survived classification.
fn by_id(&self) -> &ByIdPath {
    match self {
        AddTargetWork::Fresh(t) => &t.by_id,
        AddTargetWork::OpenRecoverable(t) => &t.by_id,
        AddTargetWork::ClosedPresentLuks(t) => &t.by_id,
    }
}
```

### 2. Build `confirm_disks` from `targets` in `build_add_credential_prelude`

Change the signature to take the classified targets and build `confirm_disks`
from them. `needs_luks_format` comes from the `Fresh` variant -- exactly
equivalent to the current `PresentNotLuks` check, since `PresentNotLuks`
classifies 1:1 to `AddTargetWork::Fresh` and fresh disks are never skipped.

```rust
fn build_add_credential_prelude(
    input: &AddStepsInput<'_>,
    targets: &[AddTargetWork],
) -> AddCredentialPrelude {
    let confirm_disks = targets
        .iter()
        .map(|target| AddConfirmDiskPlan {
            name: target.name().clone(),
            by_id: target.by_id().clone(),
            needs_luks_format: matches!(target, AddTargetWork::Fresh(_)),
        })
        .collect();
    // ... confirm_new / verify_targets / pool_target_count unchanged ...
}
```

Iterating `targets` directly (not `targets_sorted_by_name()`) preserves spec
order, which is the current confirm behavior for surviving disks -- the only
change is that no-ops are dropped. See "Alternative not taken" below.

### 3. Update the single call site

`build_add_credential_prelude` has exactly one real caller, at `add.rs:2100`
inside `build_add_work_plan`, where `targets` is already fully built:

```rust
Ok(AddWorkPlan {
    prelude: build_add_credential_prelude(input, &targets),
    targets,
    // ...
})
```

(The test at `add.rs:2853` constructs `AddCredentialPrelude { confirm_disks:
vec![], .. }` directly and is unaffected.)

### 4. Derive the success summary from work-plan targets

`AddPlan::execute` ends with `format_add_done(&self.names)` (`add.rs:1471`),
listing every parsed input name. In the mixed scenario this prints the skipped
no-op disk: the `Fresh` target keeps `journal_targets` non-empty, so execution
passes the `is_noop()` early return (`add.rs:941`) and the post-Pass-1 no-op
return (`add.rs:1117`) and reaches `:1471`.

Add a private helper on `AddWorkPlan` that returns the surviving target names in
spec order, and feed the done summary from it:

```rust
/// Disk names of the targets that survived classification, in spec
/// (input) order. Single source for the post-mutation done summary so it
/// never claims a `SameBacking` no-op disk was added.
fn target_names(&self) -> Vec<DiskName> {
    self.targets.iter().map(|t| t.name().clone()).collect()
}
```

Then change `add.rs:1471` to `format_add_done(&self.work_plan.target_names())`.
`AddWorkPlan` and `AddPlan` share the module, so the private field/method access
is fine. Spec order (not `targets_sorted_by_name()`) matches `format_add_done`'s
existing parse-order convention and keeps this a pure no-op-drop with no
reordering.

## What stays unchanged (and why)

- **`confirm_new`** (`add.rs:1824-1828`): about whether to prompt for a *new*
  passphrase, gated on `pool.devices.is_empty()` (bootstrap). At bootstrap there
  are no live members to skip, so probed and targets are identical sets -- leave
  it deriving from `input.probed`. It is correct and unrelated to the bug.
- **`verify_targets`** (`add.rs:1831-1856`): already correctly excludes
  same-UUID live members via its `pool.devices.iter().any(luks_uuid == uuid)`
  filter; built from `pool.devices` + `NoMatch` candidates. Unchanged.
- **`pool_target_count`**: unchanged.
- **`AddStepsInput.names`/`by_ids`**: still used by `verify_targets`
  (`input.names[i]`, `input.by_ids[i]`), so no field cleanup.
- **`format_add_noop` planning note** (`add.rs:1749`,
  `PreviewNote::Info(format_add_noop(&names))`): correct -- it fires only when
  `is_noop()` (every named disk is genuinely already in pool), so naming all
  input names is accurate. Unchanged.
- **`format_add_noop` execute path** (`add.rs:1119`): a defensive, effectively
  unreachable branch -- any non-empty `targets` yields non-empty
  `journal_targets` (`Fresh`/`OpenRecoverable` are pre-journaled at plan time;
  every surviving `ClosedPresentLuks` inserts on `SamePool`, the only non-error
  identity outcome -- others return `Err` via `identity_to_error`). Its "already
  in pool" wording with input names is reasonable for a "nothing got added"
  message. Leave it.
- **`AddPlan.names` field**: retained -- still used at `:1119` and `:1749`.

## Tests

Two layers: a fast planner-level unit test that pins the data-flow, and a VM
test that pins the actual CLI output at the real call sites. The unit test
alone is insufficient -- it cannot catch a revert of the `:1471` call site back
to `format_add_done(&self.names)`, because a unit-level
`format_add_done(&work_plan.target_names())` assertion exercises a composition
the production code might not use. The done line must be pinned end-to-end.

### 1. Rust unit test (planner data-flow)

Add one unit test in the `add.rs` test module, modeled on
`add_closed_present_luks_same_uuid_same_backing_drift_noops` (`add.rs:9067`)
combined with a fresh `PresentNotLuks` probed entry (shape per `add.rs:9192`).

**Test: `add_mixed_noop_and_fresh_excludes_noop_from_workplan`**

- Intent: both `confirm_disks` and `target_names()` derive from the surviving
  targets, not the raw input list.
- Why it exists: pins the data-flow fix in `build_add_credential_prelude` and
  `target_names()` cheaply and deterministically.
- Scenario: pool has a live member (uuid X @ `/dev/vdb`); plan
  `clone=usb-CLONE fresh=usb-FRESH` where `clone` is closed PresentLuks with
  uuid X and same backing (`/dev/vdb`, a no-op) and `fresh` is `PresentNotLuks`.

Setup: reuse `pool_with_live_devices` + `live_pool_device`, set `pool.fsid`,
`MockBackingPathResolver` mapping `usb-CLONE -> /dev/vdb`, `MockRunner::default()`
(neither the closed no-op nor fresh planning path issues runner probes -- the
9067 test asserts `requests().is_empty()`).

Assertions (behavioral, structure-insensitive):
- `!work_plan.is_noop()` and `work_plan.target_count() == 1` (fresh survives).
- `work_plan.prelude.confirm_disks` has exactly one entry, with
  `name == "fresh"` and `needs_luks_format == true`; no entry has
  `name == "clone"`.
- `work_plan.target_names()` equals `["fresh"]` -- excludes the no-op `clone`.

Do NOT assert on `format_add_done(&work_plan.target_names())` here: it tests a
composition the production call site may not use, giving false confidence. The
done line is owned by the VM test below.

### 2. VM test (CLI output boundary)

The confirm block renders only without `--yes` (`add.rs:946`), and the done line
is the real user-facing output at `:1471`. Pin both against live output in
`tests/cli/multi-add.py` -- the natural home: its Phase 4 is already the *pure*
no-op sibling ("Re-adding already-in-pool disks is a no-op"), and the mixed
no-op+fresh case is the missing companion. (The reviewer suggested
`braid-add-disk.py`; `multi-add.py` is the better fit -- the scenario is
inherently multi-disk, the file already has a variadic `add_cmd(*keys)`, and the
hw canary `tests/hw/test_add_canary.py` revalidates only `braid-add-disk.py`
phases 1-3, so `multi-add.py` avoids canary entanglement. `multi-add` is a
registered flake check at `flake.nix:202`.)

- Add a 6th disk image (`serial = "disk6"`) to `tests/cli/multi-add.nix`
  (`emptyDiskImages` currently holds disk1-disk5, all consumed by Phases 1-3, so
  the mixed add needs a fresh disk6).
- Add a no-`--yes` add helper feeding confirm + passphrase on one stdin stream:
  `printf 'yes\n<passphrase>\n' | braid add ... --passphrase-stdin` (the exact
  pattern proven in `tests/cli/confirm-then-passphrase-on-stdin.py`).
- New subtest after Phase 3 (disk1-5 in pool, disk6 fresh): run the mixed add
  `braid add disk1=... disk6=...` without `--yes`, redirect stderr to a file, and
  assert:
    - The "Add to pool:" block (between the `Add to pool:` header and the
      `Type 'yes' to continue:` prompt) names `disk6` and NOT `disk1`.
    - The final `Done. ... now part of the pool.` line names `disk6` and NOT
      `disk1`.
    - `btrfs fi show /mnt/storage` confirms `braid-disk6` joined.
  This fails against current code (both surfaces list `disk1`) and a revert of
  `:1471` to `&self.names` re-breaks the done-line assertion.

**Assertion-scoping caveat (must follow):** scope the "NOT `disk1`" checks to the
confirm block and the `Done.` line, not whole stderr. The credential-verify step
emits a `[wait]`/`[ok]` row per *live pool member* (`credential_verify.rs:47-60`;
`verify_targets` includes all `pool.devices`, `add.rs:1831-1844`), so stderr
legitimately names `disk1` in that section. A whole-stderr `disk1 not in err`
assertion would spuriously fail even with the fix.

The existing `format_add_confirm`/`format_add_done` formatter unit tests
(`add.rs:2297+`, `:8255-8259`) and the `yes: true` mixed test (`add.rs:7696`)
need no changes -- the fix changes the *source* feeding these formatters, not
the formatters themselves.

## Verification

1. `just test-rust` -- runs the CLI unit tests, including the new planner-level
   test and the existing add planner/confirm tests. Fast gate for the data-flow.
2. `just test-vm multi-add` -- runs the extended VM test, pinning the confirm
   block and `Done.` line at the real CLI output boundary. This is the gate that
   catches a `:1471` call-site revert (the unit test cannot).

Scope is justified: the fix touches `add`'s confirm/done output, and
`multi-add` is the directly-affected check. A full `just test-vm` run is not
warranted for this localized change; leave the unscoped suite to the user.

## Alternative not taken

Sorting `confirm_disks` by name (iterating `targets_sorted_by_name()`) would
make the confirm prompt's order match the dry-run preview's order. Rejected:
it changes existing confirm ordering for the unrelated all-surviving multi-disk
case, and `confirm_disks` is already deterministic in spec order (unlike the
UUID-keyed preview, which sorts to avoid randomness). Preserving spec order
keeps the blast radius to exactly "drop no-ops."

## Files touched

- `cli/src/add.rs` -- `by_id()` accessor on `AddTargetWork` and `target_names()`
  helper on `AddWorkPlan`; `confirm_disks` construction + signature in
  `build_add_credential_prelude`; its call site at `:2100`; the `format_add_done`
  call at `:1471`; one new planner-level unit test.
- `tests/cli/multi-add.py` -- new mixed no-op+fresh subtest asserting the confirm
  block and `Done.` line name only the fresh disk (block/line-scoped).
- `tests/cli/multi-add.nix` -- add a 6th disk image (`serial = "disk6"`).
