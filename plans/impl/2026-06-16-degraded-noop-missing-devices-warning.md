# Fix: suppress the missing-devices warning on a no-op re-add

## Context

On a **degraded** pool (one or more members MISSING), running
`braid add disk1=...` where `disk1` is **already a pool member** is a no-op.
Today the preview/output still prints the action-implying missing-devices
warning alongside the no-op note:

```
[warn] pool has 1 missing device. Consider repairing with `braid replace ...` first. ...
Nothing to do -- disk1 already in pool.
```

The warning says "Consider repairing ... **first**" -- advice that only makes
sense before a *real* add. On a no-op nothing happens, so the warning
contradicts the clean "Nothing to do" signal.

**Root cause** (`cli/src/add.rs`, `plan_add`): the missing-devices `Warn` is
pushed at `cli/src/add.rs:1820` gated only on `pool.missing_count > 0`,
*before* `build_add_work_plan` runs, so it fires regardless of whether the add
is a no-op. Its two sibling notes are already no-op-aware -- the
keyfile-asymmetry warning derives from `work_plan.targets` (empty on a no-op)
and the degraded-balance `Skip` is gated on `!work_plan.is_noop()`
(`cli/src/add.rs:1897`). The missing-devices warning is the lone straggler;
this fix brings it in line. (The `!is_noop()` guard on the sibling was added by
`1b5d6965 feat(add): skip the RAID1 convert balance on a degraded add`, which
did not touch this warning.)

**Constraint the naive fix misses:** the warning is currently pushed *before*
the work-plan build specifically so it survives on `PlanFailure::notes` when
the build fails (a degraded-pool refusal must still show the operator the pool
is degraded). This is pinned by `plan_add_preserves_warn_notes_on_later_failure`
(`cli/src/add.rs:10016`). Simply moving the push after the build -- as the
finding phrases it -- would drop the warning from the failure path and break
that test. The fix must keep the warning on the **refusal** path while
suppressing it only on a **successful no-op**.

## The fix (production code)

**File:** `cli/src/add.rs`, function `plan_add`, the block currently spanning
the missing-devices push (`:1816-1824`) and the work-plan build (`:1826-1850`).

Reorder so the build runs first, then push the warning with a single condition
that suppresses it only on a successful no-op. Replace the two blocks with:

```rust
    // Build the semantic work plan first so the missing-devices warning can
    // see whether real work will happen. This can fail on PresentLuks identity
    // / foreign-pool guards.
    let by_ids_refs: Vec<&ByIdPath> = by_ids.iter().collect();
    let work_plan_result = build_add_work_plan(
        runner,
        &AddStepsInput {
            names: &names,
            by_ids: &by_ids_refs,
            probed: &probed,
            pool: &pool,
            mount_point: config.mount_point(),
            paths: params.paths,
            enroll_key_file: params.enroll_key_file,
            luks_format_extra_opts: &luks_format_extra_opts,
            backing_path_resolver: params.backing_path_resolver,
            pool_membership: &pool_membership,
        },
    );

    // Missing-devices warning: body-only, no legacy `warning:` prefix. Lives on
    // `notes` so it surfaces on both dry-run stdout (via `Preview::render`) and
    // real-run stderr (via `AddPlan::execute` using
    // `preview::render_notes_for_stderr`). Emitted for a real add AND for a
    // planner refusal (a degraded pool is context the operator needs to make
    // sense of the refusal -- pinned by
    // `plan_add_preserves_warn_notes_on_later_failure`), but SUPPRESSED on a
    // successful no-op re-add: nothing happens, so "consider repairing first"
    // would mislead. Mirrors the no-op suppression the keyfile-asymmetry and
    // degraded-balance-skip notes already apply.
    let suppress_missing_warn =
        matches!(&work_plan_result, Ok(work_plan) if work_plan.is_noop());
    if pool.missing_count > 0 && !suppress_missing_warn {
        notes.push(PreviewNote::Warn(format_add_missing_devices_warning(
            pool.missing_count,
        )));
    }

    // Accumulated notes (missing-devices) must survive on `PlanFailure::notes`
    // so the caller can render them to stderr before the error.
    let work_plan = match work_plan_result {
        Ok(s) => s,
        Err(e) => {
            return Err(PlanFailure::with_notes(notes, e));
        }
    };
```

Why this shape (single push, `matches!`):
- **Refusal path:** `work_plan_result` is `Err`, so `suppress_missing_warn` is
  `false` and the warning is pushed before the `Err` return -- failure-path
  context preserved.
- **No-op success:** `Ok` + `is_noop()` -> suppressed.
- **Real add success:** `Ok` + `!is_noop()` -> pushed, unchanged.
- **Ordering:** the push still lands before the keyfile block
  (`cli/src/add.rs:1854`), so the missing-before-keyfile note order holds.
- One push site / one `format_add_missing_devices_warning` call -> the two
  intents can't drift.

No change to `format_add_missing_devices_warning` (`cli/src/add.rs:942`) or any
note wording. `build_add_work_plan` has no side effect on `notes`, and nothing
else sits between the old push and the build, so the reorder is otherwise
behavior-preserving. (UPS-preflight and earlier refusals at
`cli/src/add.rs:1810` already run before this block and already do not carry the
missing-devices warning -- unchanged.)

## The test (new regression guard)

No existing test combines a degraded pool with a no-op re-add. Add one unit
test in the `plan_add` test module of `cli/src/add.rs`, modeled exactly on the
proven no-op test `plan_add_keyfile_no_warn_when_target_already_in_pool_with_empty_slot_1`
(`cli/src/add.rs:9556`) -- the only change is `.with_missing(1)`.

Suggested name: `plan_add_noop_on_degraded_pool_omits_missing_devices_warning`.

Runner setup (two keyfile probes make both `braid-disk1` and `braid-disk2` real
pool members; `.with_missing(1)` appends a MISSING row so
`missing_count = total_devices(3) - devices.len(2) = 1`, per
`cli/src/probe.rs:498`; `AlreadyInPoolSlot1Empty` makes `disk2` a SameBacking
no-op via `classify_live_pool_match` at `cli/src/add.rs:271`):

```rust
let fixture = plan_add_fixture();
let fs = AddMockFs(vec!["/dev/disk/by-id/virtio-disk2".into()]);
let runner = AddPlanTestRunner::new()
    .with_keyfile_probes(vec![
        AddPlanKeyfileProbe::Occupied,
        AddPlanKeyfileProbe::Empty,
    ])
    .with_missing(1)
    .with_target_probe(
        "/dev/disk/by-id/virtio-disk2",
        AddPlanTargetProbe::AlreadyInPoolSlot1Empty,
    );

let disk_specs = ["disk2=/dev/disk/by-id/virtio-disk2".to_string()];
let plan = plan_add(&runner, &fs, &fixture.params(&disk_specs, true))
    .expect("plan_add should succeed on a degraded no-op re-add");
```

Assertions -- pin the **exact rendered boundary**, mirroring
`plan_add_already_in_pool_is_note_only_success` (`cli/src/add.rs:9712-9725`).
Asserting the full render (not just filtering `plan.notes` by variant) is the
stronger guard: a `warns.is_empty()` + one-`Info` check would still pass if a
non-`Warn` action-context note -- e.g. the degraded-balance `[skip]`
(`PreviewNote::Skip`) -- leaked onto a degraded no-op. The render equality
catches any such leak.

```rust
// Precondition: the pool really is degraded, so the clean render below
// demonstrates suppression rather than passing vacuously on a healthy pool.
assert_eq!(plan.pool.missing_count, 1, "test must exercise a degraded pool");

let preview = plan.preview();
assert!(
    preview.steps.is_empty(),
    "no-op must have zero steps, got: {:?}",
    preview.steps
);

// Exact boundary: only the no-op line, no `[warn]`/`[skip]` action-context
// note. This is the regression line -- on today's code the render carries the
// `[warn] pool has 1 missing device ...` line and the assert fails.
let rendered = preview.render();
assert_eq!(
    rendered,
    "Nothing to do -- disk2 already in pool.\n",
    "degraded no-op must render only the no-op line"
);
assert!(
    !rendered.contains("nothing to do."),
    "generic `nothing to do.` fallback must NOT appear alongside the Info note"
);
```

(Under the fix this equals the healthy-pool no-op render because the
missing-devices warning is suppressed and the degraded-balance `[skip]` is
already gated on `!work_plan.is_noop()` at `cli/src/add.rs:1897`, so no
action-context note fires.)

Add the standard `//` preamble (Intent / Why it exists / Scenario) used by the
neighbouring tests.

## Contracts preserved (must still pass, unchanged)

- `plan_add_preserves_warn_notes_on_later_failure` (`:10016`) -- refusal keeps
  the missing-devices warning.
- `plan_add_missing_devices_becomes_single_warn_note` (`:9188`) -- real add to a
  degraded pool still warns.
- `plan_add_warn_notes_preserve_missing_before_keyfile_order` (`:9305`) -- note
  order unchanged.
- `plan_add_keyfile_no_warn_when_target_already_in_pool_with_empty_slot_1`
  (`:9556`) and `plan_add_already_in_pool_is_note_only_success` (`:9675`) --
  healthy-pool no-ops unchanged.

## Docs / ADR

No change required. `docs/commands/add.md` describes the missing-devices warning
only for the real-add path ("`braid add` still adds the new disk ... skips the
RAID1 convert balance"); it makes no claim about the no-op case, and the fix
keeps real-add behavior identical. This is an output-correctness fix, not an
invariant or principle change, so `principles.md` / `decisions/` are untouched.
Warning wording is unchanged, so the ASCII-output check is unaffected (and test
code is exempt regardless).

## Verification

1. `just test-rust` -- runs the `cli` Rust unit tests, including the new test
   and all preserved contracts above. (Targeted: `cargo test -p braid-cli
   plan_add_noop_on_degraded` and `cargo test -p braid-cli plan_add`.)
2. Confirm the new test **fails before** the production edit and **passes
   after** (write-test-first / confirm-it-fails-for-the-right-reason): before
   the fix the render-equality assert fails because the output carries the extra
   `[warn] pool has 1 missing device ...` line ahead of the no-op line.
3. `cargo clippy` clean (the `matches!` guard and reorder introduce no new
   lints).
4. No parser/fixture impact (no change to parsed tool output), so the
   fixture-refresh lanes are not triggered.
