# Optional Work-Plan Cleanup

## Summary

Clean up the migrated work-plan commands after the unification commit. Keep
scope limited to `add`, `replace`, `remove`, `remove-missing`, and `recover`;
do not refactor `lock`, `unlock`, or `enroll`.

The cleanup should not change CLI behavior or rendered dry-run output. It should
remove stale internal affordances that make it look like preview steps are still
cached or compiled separately.

## Key Changes

- Remove cached `pub steps: Vec<Step>` fields from the migrated plan structs.
  - `preview()` remains the only way to get rendered steps from a plan.
  - `preview()` must continue to render steps from the command's
    `work_plan.render_steps()`.
  - Remove construction-time `let steps = work_plan.render_steps()` where it
    only exists to fill the cached field.
  - For `add` no-op detection, use `work_plan.is_noop()` instead of rendering
    steps just to check emptiness.

- Retire migrated test-only render wrappers.
  - Remove `render_add_work_plan_steps`, `compile_replace_steps`,
    `compile_remove_present_steps`, and `remove_missing`'s test-only
    `compile_steps`.
  - Move dry-run output and plan-behavior assertions to the public command
    boundary: call `plan_*()`, then assert on `plan.preview().render()` or
    `plan.preview().steps`.
  - Use direct `work_plan.render_steps()` only for narrow leaf-renderer tests
    that are not cheaply reachable through `plan_*().preview()`.
  - If fixture setup would become noisy, keep narrowly named test builders that
    return a command plan or work plan, not rendered steps; behavior-facing
    tests should still assert through `preview()`.

- Update tests that inspect `plan.steps`.
  - Use `plan.preview().steps` when the assertion is about the step list.
  - Use `plan.preview().render()` when the assertion is about user-visible
    output.
  - Replace `Step::render_dry_run(&plan.steps)` comparisons with
    `Step::render_dry_run(&plan.preview().steps)` only for no-note
    byte-equivalence tests.

- Update comments to remove stale "cached preview output" and old `compile_*`
  references in migrated command modules.
  - Leave `compile_open_steps`, `compile_lock_steps`, and
    `compile_enroll_steps` unchanged; they are outside this cleanup scope.

## Test Plan

- Run `cargo fmt`.
- Run `just test-rust`.
- Run `git diff --check`.
- Verify cleanup mechanically:
  - `rg "pub steps: Vec<Step>|plan\\.steps|compile_replace_steps|compile_remove_present_steps|render_add_work_plan_steps|fn compile_steps" cli/src/add.rs cli/src/replace.rs cli/src/remove.rs cli/src/remove_missing.rs cli/src/recover.rs` should return no migrated-command leftovers.
- No VM tests are required unless unit tests reveal a behavior change, because
  this cleanup should only alter internal test seams and cached fields.

## Assumptions

- Scope is "migrated polish": no work-plan rewrite for `lock`, `unlock`, or
  `enroll`.
- `Preview.steps` remains unchanged; only migrated command plan structs lose
  cached `steps`.
- Dry-run stdout, real-run stderr, execution behavior, and journal/recovery
  semantics must remain byte-for-byte compatible with the current committed
  behavior.
- Existing unrelated dirty or untracked files stay untouched.
