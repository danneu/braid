# Pin the `just_mounted == false` defense-in-depth gates in recover execute

## Context

`RecoverWorkAction::WaitForKernelReplace::execute` (`cli/src/recover.rs:439-449`)
and the sibling `RecoverWorkAction::RemountCycle::execute`
(`cli/src/recover.rs:450-469`) both guard a `if state.just_mounted` branch:

- `WaitForKernelReplace` calls `wait_for_kernel_replace_to_finish` when true,
  no-ops when false.
- `RemountCycle` calls `relock_and_remount` when true, no-ops when false.

In production these gates are pure defense-in-depth on top of two upstream
guards in `plan_recover`:

- `cli/src/recover.rs:1310-1326` -- refuses `Replace::PoolMutation` recovery
  when `open_plan.is_none()` (already-mounted pool). Pinned by
  `plan_recover_refuses_replace_on_externally_mounted_pool` at
  `cli/src/recover.rs:15949`.
- `cli/src/recover.rs:1381-1383` and `:1406-1410` -- only push
  `WaitForKernelReplace` / `RemountCycle` into the work plan when
  `open_plan.is_some()`.

The execute-time gates exist to defend the TOCTOU window where a plan is built
against an unmounted pool but `mount(8)` at execute time returns
"already mounted" (`Ok(false)`), so `state.just_mounted` ends up false. In that
narrow window, running the wait or the remount cycle against a mount we did
not open would be unsafe.

The eleven `wait_for_kernel_replace_*` tests
(`cli/src/recover.rs:3958-4392`) all call the freestanding
`wait_for_kernel_replace_to_finish` directly; they bypass the gate. The
only test that drives `RecoverWorkAction::WaitForKernelReplace.execute`
through the planner+execute loop is `recover_replays_resize_after_replace_via_mount_cycle`
(`cli/src/recover.rs:14187`), and it exercises only the
`just_mounted == true` path. The same gap exists for `RemountCycle`. A
regression that flipped either gate (`if !state.just_mounted`) or removed it
outright would compile and pass `just test-rust`.

This plan pins both execute-time gates with two small behavioral unit tests.
Pinning only `WaitForKernelReplace` (the finding's literal prescription)
would leave the identical sibling invariant uncovered.

## Approach

Add two Rust unit tests in the existing `mod tests` block in
`cli/src/recover.rs`, placed adjacent to
`recover_replays_resize_after_replace_via_mount_cycle`
(`cli/src/recover.rs:14187`), so the `just_mounted == true` cycle test and
the `just_mounted == false` no-op tests sit together.

Each test:

1. Builds a `RecoverWorkPlan` for a replace-pool-mutation journal using the
   existing `recover_work_plan_for_journal` helper at
   `cli/src/recover.rs:4747`, then **sets `plan.open_plan = Some(OpenPlan {
   ... })`** to model the real TOCTOU shape -- the action is only ever
   pushed onto the plan when `open_plan.is_some()` (`recover.rs:1381`,
   `:1406`), and the gate only fires in the narrow window where execute
   then observed the pool already mounted. A minimal `OpenPlan` (`mount.rs:97`,
   4 fields: `to_unlock`, `any_open`, `any_missing_member`, `mount_device`)
   suffices; neither arm reads `plan.open_plan` on the no-op branch, so
   exact field values are inert.
2. Constructs `RecoverExecutionState { credential: None, just_mounted: false }`
   directly. Safe because both arms early-return before any access to
   `state.credential`.
3. Uses `MockRunner::default()` -- empty. Critically, the request log is
   the load-bearing assertion (see step 6), not the return value:
   `wait_for_kernel_replace_to_finish` swallows runner errors at
   `cli/src/recover.rs:3369-3376` and returns `Ok(())`, so a flipped or
   removed gate would still propagate as `Ok(false)` from the action arm
   and pass a naive `matches!(...)` assert.
4. Uses `MockFs::new(&[])` and `resolver_for(&[])` (`cli/src/by_id.rs:130`)
   for the filesystem / by-id wiring.
5. Builds `RecoverParams` via `f.recover_params().build()` from
   `PoolFixture::empty()`.
6. Calls the action's `execute(...)` arm directly and asserts BOTH:
   - `matches!(result, Ok(false))`
   - `runner.requests().is_empty()` (via `MockRunner::requests()` at
     `cli/src/cmd.rs:1454`)

   The request-log assertion is the structure-insensitive pin on the
   "no runner interaction" invariant. It catches a flipped or removed gate
   even though `wait_for_kernel_replace_to_finish` swallows runner errors,
   and it stays correct if a future refactor changes how either
   downstream helper handles runner failures.

### Test 1: `wait_for_kernel_replace_no_ops_when_just_mounted_false`

```rust
// Intent: RecoverWorkAction::WaitForKernelReplace.execute returns
// Ok(false) without touching the runner when state.just_mounted is false.
//
// Why it exists: The `if state.just_mounted` gate at recover.rs:440 is
// defense-in-depth on top of plan_recover's already-mounted refusal
// (recover.rs:1310, pinned by
// plan_recover_refuses_replace_on_externally_mounted_pool) and its
// `open_plan.is_some()` push gate (recover.rs:1381). Without this test, a
// regression that flips the gate (`if !state.just_mounted`) or removes it
// would compile and pass `just test-rust`, leaving production safety
// dependent solely on the planner refusal.
//
// Scenario: TOCTOU window -- plan_recover saw an unmounted pool and
// produced `open_plan: Some(_)`, but by execute time the mount call
// observed the pool already mounted and returned `Ok(false)`, so
// `state.just_mounted` ended up false. WaitForKernelReplace must not
// probe `btrfs replace status` on a mount session we did not open.
```

Body: construct the artifacts above; invoke
`RecoverWorkAction::WaitForKernelReplace.execute(&plan, &mut state, &runner, &fs, &resolver, &params)`;
assert both `matches!(result, Ok(false))` and `runner.requests().is_empty()`.
The latter is load-bearing: `wait_for_kernel_replace_to_finish`
(`cli/src/recover.rs:3354`) swallows runner errors and returns `Ok(())` at
`:3369-3376`, so a regression that flipped the gate would still surface as
`Ok(false)` from the action arm without the request-log check.

### Test 2: `remount_cycle_no_ops_when_just_mounted_false`

```rust
// Intent: RecoverWorkAction::RemountCycle.execute returns Ok(false)
// without touching the runner when state.just_mounted is false.
//
// Why it exists: Same defense-in-depth pattern as
// WaitForKernelReplace -- the `if state.just_mounted` gate at
// recover.rs:451 guards relock_and_remount (umount + scan-forget +
// LUKS close+reopen + remount), all backstopped by the planner's
// `open_plan.is_some()` push gate at recover.rs:1406. A regression
// here would attempt to umount a foreign mount session.
//
// Scenario: Same TOCTOU window as the WaitForKernelReplace no-op
// test. The remount cycle must not run when recover did not open the
// mount itself.
```

Body: same artifacts; invoke

```rust
RecoverWorkAction::RemountCycle {
    close_names: vec![DiskName("braid-disk1".into())],
    reopen_names: vec![DiskName("braid-disk1".into())],
    any_missing_member: false,
}
.execute(&plan, &mut state, &runner, &fs, &resolver, &params)
```

assert both `matches!(result, Ok(false))` and `runner.requests().is_empty()`.
The specific `close_names` / `reopen_names` content is inert on this path
-- they are only read inside the `if state.just_mounted` branch. The
request-log assertion gives this test the same structure-insensitive shape
as the `WaitForKernelReplace` test, even though `relock_and_remount`
propagates runner errors via `.map_err` (unlike the wait helper) -- the
uniform shape protects both tests against future refactors that might
change either downstream helper's error-handling.

## Critical files

- `cli/src/recover.rs` -- only file modified. Add two `#[test]` functions
  inside the existing `mod tests` block, adjacent to
  `recover_replays_resize_after_replace_via_mount_cycle`
  (around `cli/src/recover.rs:14187`).

No other files change. Helpers reused (all already in the test mod or in
sibling crates):

- `recover_work_plan_for_journal` -- `cli/src/recover.rs:4747`
- `replace_journal()` (or equivalent journal builder used at
  `cli/src/recover.rs:14190`)
- `OpenPlan` -- `cli/src/mount.rs:97`
- `PoolFixture::empty()` and `f.recover_params().build()`
- `MockRunner::default()` and `MockRunner::requests()` -- `cli/src/cmd.rs:1454`
- `MockFs::new(&[])` -- `cli/src/recover.rs:3769`
- `resolver_for(&[])` -- `cli/src/by_id.rs:130`

## Verification

1. `just test-rust` -- both new tests must pass against the current code.
2. Sanity-check the regression catches by mentally flipping each gate to
   `if !state.just_mounted`. With the gate flipped:
   - `WaitForKernelReplace`: the action calls `wait_for_kernel_replace_to_finish`,
     which calls `runner.run(&CmdRequest::BtrfsReplaceStatus { ... })`. The
     empty `MockRunner` returns `Err(CmdError::MissingMock)`, the helper
     swallows it and returns `Ok(())` (`recover.rs:3369-3376`), the action
     arm returns `Ok(false)`. The `matches!(result, Ok(false))` assert
     would pass -- but `runner.requests().is_empty()` fails because the
     `BtrfsReplaceStatus` request was logged. The test catches the regression.
   - `RemountCycle`: the action calls `state.credential.as_ref().expect(...)`
     before reaching `relock_and_remount` (`recover.rs:454`), which panics
     because we constructed `credential: None`. The test fails with a
     panic. Even if a future refactor moved the credential check past
     `runner.run(...)`, `runner.requests().is_empty()` still catches it.
3. No NixOS VM tests required. The added coverage is execute-arm-local,
   structure-insensitive, and behavioral.

## Out of scope

- No changes to production code (`recover.rs:439-469`). The gates are
  already correct -- this plan is test-only.
- No refactor of the existing `wait_for_kernel_replace_*` tests. They test
  the freestanding helper and remain useful as-is.
- No new planner-level test. `plan_recover_refuses_replace_on_externally_mounted_pool`
  (`cli/src/recover.rs:15949`) already covers the upstream refusal.

## Implementation notes

- The plan's literal `DiskName("braid-disk1".into())` syntax in Test 2's
  body does not compile -- `DiskName`'s single field is private. Used the
  existing `disk_name("disk1")` helper (`cli/src/recover.rs:4482`) instead.
  Inert per the plan ("only read inside the `if state.just_mounted`
  branch"), and `disk1` matches the semantic shape (close_names hold disk
  names, not mapper names -- `config::mapper_name` prepends `braid-` at
  use sites in the gated branch).
- Verified the regression catch empirically by flipping each gate to
  `if !state.just_mounted` and re-running both tests; both failed as the
  plan's Verification section predicted (`WaitForKernelReplace` failed on
  the request-log assert; `RemountCycle` panicked on the credential
  `.expect`). Reverted before staging.
