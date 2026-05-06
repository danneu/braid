# Unify CLI Plan And Execute Paths

## Summary

Refactor the remaining braid mutating commands that still build dry-run preview
steps separately from the real execution path. The goal is the same shape as the
`add` refactor: each command should compile one semantic work plan, render
human-readable preview steps from that plan, and execute by consuming the same
plan.

This reduces drift between `--dry-run` output and real behavior for risky disk
operations. It also makes tests more meaningful: preview tests can assert the
user-facing plan, while execution tests assert that the same plan drives the
actual btrfs, cryptsetup, journal, and cleanup behavior.

## Commands That Need This Refactor

1. `replace`
   - Priority: high.
   - Current shape: `ReplacePlan` owns preview-only `steps`, while execution
     separately performs journal writes, LUKS work, `btrfs replace`, resize,
     cleanup, and optional balance work.
   - Risk: the preview compiler can drift from the real replacement sequence,
     especially around LUKS formatting/opening, passphrase handling, old-device
     cleanup, and soft balance behavior.

2. `remove-missing`
   - Priority: medium.
   - Current shape: `RemoveMissingPlan` owns preview-only `steps`, while
     execution separately writes the journal, runs missing-device removal, and
     optionally starts a RAID1 soft balance.
   - Risk: the dry-run can miss journal and post-remove work that affects
     recovery semantics.

3. `remove`
   - Priority: medium to low.
   - Current shape: `RemovePlan` owns preview-only `steps`, while execution
     separately evicts the present device and closes the LUKS mapping.
   - Risk: smaller than `replace`, but preview and execution can still diverge
     around open mapper cleanup, mounted-pool requirements, and optional balance
     work.

4. `recover`
   - Priority: high, but do after the operation-specific commands.
   - Current shape: `RecoverPlan` owns preview-only `steps`, while execution
     dispatches by journal operation and replays separate recovery logic.
   - Risk: recovery preview is especially sensitive because users rely on it
     when the system is already in an exceptional state. Recovery also has
     safety behavior that is not just original-command replay: replace remount
     cycling, phase-specific post-maintenance, live-pool reconciliation, and
     paused-balance policy.

## Commands That Do Not Need This Refactor Now

1. `unlock`
   - Already has an `OpenPlan` that both preview and execution consume.

2. `enroll`
   - Already has action-oriented planning with `DiskEnrollAction` and
     `apply_enrollment`.

3. `lock`
   - Simple enough that a larger plan/execution rewrite is not justified.
     Runtime rechecking is intentional because device state can change before
     locks are closed.

4. Read-only or non-dry-run commands
   - `status`, `doctor`, `monitor`, `ack`, `idle`, `scrub-*`, `ups`, `browse`,
     and `discover` do not have the same dry-run/execute split and are not part
     of this roadmap.

## Work Plan Rules

- [ ] Keep `Step` as a user-facing render type. Do not turn it into the
      execution language.
- [ ] Put executable intent in typed work-plan actions.
- [ ] Allow runtime-checked actions as first-class plan entries when the current
      command intentionally re-probes or inspects live state before acting.
      These entries must name their guard in preview text and enforce the same
      guard at execution time.
- [ ] Preserve runtime guards that prevent stale decisions, including:
      - `remove` re-probes before eviction and no-ops if the mapper is no
        longer in the live pool
      - post-degraded RAID1 restore probes after mutation before deciding
        whether to run a soft balance
- [ ] Test runtime-checked actions through behavior: dry-run should describe the
      conditional work, and execution tests should prove both the run and skip
      branches where the branch matters for safety.

## Implementation Plan

### Phase 0: Finish And Verify `add`

- [ ] Treat `add` as the reference implementation for the rest of the roadmap.
- [ ] Confirm the final `AddWorkPlan` owns every semantic operation needed by
      both preview and execution.
- [ ] Remove or retire legacy preview-only helpers such as
      `compile_add_steps_multi` once no production path needs them.
- [ ] Before deleting `compile_add_steps_multi`, migrate its behavior coverage
      to `AddPlan::preview`, `AddWorkPlan::render_steps`, or equivalent
      command-level behavior tests. Preserve coverage for labeled-LUKS
      rejection, deferred closed-LUKS identity verification, dry-run command
      rendering, LUKS header backup ordering, and keyfile enrollment ordering.
- [ ] Keep preview rendering as a method on the semantic plan, not as a second
      planner.
- [ ] Keep phased execution boundaries explicit:
      - pre-journal work
      - journal registration
      - pool mutation
      - post-mutation cleanup
- [ ] Re-run focused `add` tests, including returned-disk, locked-pool,
      passphrase mismatch, LUKS header backup, and recovery cases.

### Phase 1: Refactor `replace`

- [ ] Introduce a semantic `ReplaceWorkPlan`.
- [ ] Move replacement decisions into typed target/action data:
      - old device identity and mapper state
      - new device preparation mode
      - journal target
      - LUKS format/open/enroll work
      - `btrfs replace start` work
      - resize work
      - old-device close/cleanup work
      - optional RAID1 soft balance work
- [ ] Make dry-run render from `ReplaceWorkPlan`.
- [ ] Make execution consume `ReplaceWorkPlan`.
- [ ] Delete or reduce `compile_replace_steps` after the render path is covered
      by the new plan.
- [ ] Preserve behavior for passphrase preflight, preformatted LUKS mismatch,
      new-device-in-pool guards, replacement journal recovery, and suspend
      inhibition.
- [ ] Add or update unit tests around plan rendering where the old
      `compile_replace_steps` tests were providing value.
- [ ] Run focused VM tests:
      - `replace-live-disk`
      - `replace-live-disk-busy`
      - `replace-dead-disk`
      - `replace-larger-disk`
      - `replace-2disk-pool`
      - `replace-inhibits-suspend`
      - `replace-luks-label`
      - `replace-sequential`
      - `replace-new-already-luks`
      - `replace-passphrase-mismatch`
      - `replace-preformatted-luks-passphrase-mismatch`
      - `replace-new-in-pool-guard`
      - `replace-preserves-devid`
      - `replace-preview-warnings`
      - `recover-replace-not-started`
      - `recover-replace-completed`

### Phase 2: Refactor `remove-missing`

- [ ] Introduce a semantic `RemoveMissingWorkPlan`.
- [ ] Represent the journal record, missing-device removal operation, and
      optional post-remove soft balance as typed plan entries.
- [ ] Make dry-run render from `RemoveMissingWorkPlan`.
- [ ] Make execution consume `RemoveMissingWorkPlan`.
- [ ] Delete or reduce the old preview-only `compile_steps` helper.
- [ ] Preserve recovery semantics for crashes before, during, and after missing
      device removal.
- [ ] Run focused VM tests:
      - `remove-missing-inhibits-suspend`
      - `braid-remove-missing-enospc`
      - `braid-remove-missing-enospc-crash`
      - `braid-remove-missing-softwarn`
      - `remove-missing-membership-readonly`
      - `recover-remove-missing-completed`

### Phase 3: Refactor `remove`

- [ ] Introduce a semantic `RemoveWorkPlan`.
- [ ] Represent present-device removal, mapper close, and optional balance work
      as typed plan entries.
- [ ] Make dry-run render from `RemoveWorkPlan`.
- [ ] Make execution consume `RemoveWorkPlan`.
- [ ] Delete or reduce `compile_remove_present_steps`.
- [ ] Preserve mounted-pool checks, busy-device handling, ENOSPC behavior,
      metadata profile warnings, and suspend inhibition.
- [ ] Preserve the runtime-checked eviction guard: execution must re-probe the
      live pool immediately before eviction and no-op if the target mapper is no
      longer present.
- [ ] Run focused VM tests:
      - `braid-remove-disk`
      - `remove-no-membership`
      - `remove-metadata-dup`
      - `remove-inhibits-suspend`
      - `braid-remove-disk-busy`
      - `braid-remove-enospc`
      - `braid-remove-softwarn`
      - `braid-recover-remove`

### Phase 4: Refactor `recover`

- [ ] Refactor recovery planning only after `add`, `replace`, `remove`, and
      `remove-missing` expose semantic work plans.
- [ ] Introduce a recovery-native `RecoverWorkPlan`. It may reuse leaf action
      types from command work plans, but it must not be a wrapper around the
      original command plans.
- [ ] Make recovery dry-run render from `RecoverWorkPlan`.
- [ ] Make recovery execution consume `RecoverWorkPlan`.
- [ ] Keep journal operation and phase dispatch explicit, but make each branch
      produce recovery-native semantic actions instead of preview-only steps
      plus separate execution logic.
- [ ] Model the replace recovery relock/remount cycle as typed recovery work:
      - wait for kernel dev_replace resume when recovering a replace that was
        mounted by `recover`
      - unmount the recovery mount
      - run `btrfs device scan --forget`
      - close every mapper needed to clear stale kernel fs-device state
      - re-open the membership union with the resolved credential
      - rescan and remount with the same degraded policy
      - abort before writing `pool.json` or clearing the journal if the cycle
        fails
- [ ] Model phase-specific post-maintenance as typed recovery work:
      - Add post-balance recovery may resume an owed paused balance and replay
        the soft RAID1 balance
      - Replace post-maintenance may replay resize and owed RAID1 maintenance
      - RemoveMissing post-maintenance may replay owed RAID1 maintenance
      - Remove recovery must not resume a paused balance or replay soft RAID1
        maintenance
- [ ] Model live-pool reconciliation as typed recovery work. Rebuild membership
      from the mounted pool's actual topology and fail rather than guessing when
      a live device cannot be resolved to a stable `/dev/disk/by-id/` path.
- [ ] Preserve dry-run placeholders for conditional recovery actions. Preview
      must show conditional work without rendering command lines for actions
      whose execution depends on post-mount state, such as paused-balance
      resume, soft RAID1 replay, replace resize replay, and the replace kernel
      wait. The relock/remount cycle is different: when `recover` will perform
      it, dry-run must render the concrete unmount, scan, close, open, rescan,
      and mount commands.
- [ ] Preserve the explicit paused-balance policy: Add, Replace, and
      RemoveMissing may resume or replay owed maintenance; Remove must leave an
      ambiguous paused pre-remove balance alone so the operator can rerun
      `braid remove`.
- [ ] Preserve idempotence for already-completed operations and interrupted
      operations.
- [ ] Keep these recovery unit tests as phase gates:
      - `recover_remount_cycle_umount_failure_aborts_before_pool_json`
      - `plan_recover_dry_run_includes_remount_cycle_when_not_mounted`
      - `plan_recover_dry_run_omits_remount_cycle_when_already_mounted`
      - `plan_recover_dry_run_replace_not_mounted_includes_dev_replace_wait`
      - `plan_recover_dry_run_add_not_mounted_omits_dev_replace_wait`
      - `plan_recover_dry_run_cycle_close_set_includes_absent_open_mapper`
      - `plan_recover_dry_run_cycle_reopen_set_excludes_damaged_header_disk`
      - `plan_recover_dry_run_cycle_mount_uses_first_reopen_not_initial_mount_device`
      - `plan_recover_dry_run_replace_replay_placeholders_have_no_commands`
      - `plan_recover_dry_run_remove_missing_post_mutation_placeholders_are_shown`
      - `plan_recover_dry_run_add_post_mutation_placeholders_have_no_commands`
      - `plan_recover_dry_run_post_add_balance_only_has_no_target_replay`
      - `plan_recover_dry_run_remove_post_mutation_replay_rows_omitted`
      - `recover_resumes_paused_balance_then_clears_journal`
      - `recover_skips_paused_balance_resume_for_remove`
      - `replace_post_maintenance_skips_unowed_balance`
      - `replace_post_maintenance_runs_owed_balance`
      - `remove_missing_post_maintenance_skips_unowed_balance`
- [ ] Run focused VM tests:
      - `braid-recover`
      - `braid-recover-remove`
      - `recover-remove-missing-completed`
      - `recover-bootstrap-crash`
      - `recover-replace-not-started`
      - `recover-replace-completed`

## Commit Slicing

- [ ] Treat phases as implementation checkpoints, not necessarily single
      commits. Each commit should compile, keep the touched command's focused
      Rust tests passing, and avoid mixing unrelated commands.
- [ ] Suggested commit slices:
      - `refactor(add): retire legacy add preview helpers`
      - `refactor(replace): introduce replace work plan`
      - `refactor(replace): execute replace from work plan`
      - `refactor(remove-missing): introduce remove-missing work plan`
      - `refactor(remove): execute remove from work plan`
      - `refactor(recover): introduce recovery-native work plan`
      - `refactor(recover): execute recover from recovery work plan`
- [ ] Split a suggested slice further if it touches both planner structure and
      risky execution behavior in a way that would make review difficult.
- [ ] Run each phase's focused VM gates before moving to the next phase, even
      when the phase is split across multiple commits.

## Test Strategy

- [ ] Keep tests behavioral and structure-insensitive. Assert what the user sees
      or what the command actually does, not private enum layout.
- [ ] For each command, keep or add preview tests for warnings, safety gates,
      passphrase preflight, journal boundaries, and optional balance work.
- [ ] For each command, keep or add execution tests for the same behaviors that
      preview claims will happen.
- [ ] For runtime-checked actions, test both the guard description and the live
      execution guard. Do not replace a live-state check with a static planning
      decision just to make preview and execution easier to compare.
- [ ] Run `just test-rust` after each command refactor.
- [ ] Run focused `just test-vm` checks for each phase.
- [ ] Run the union of affected VM checks before considering the roadmap
      complete.

## Notes

- Do not add backwards-compatibility shims for old internal plan structures.
  braid is unreleased; update callers and tests together.
- Keep CLI output ASCII-only. Use `--`, not an em dash, in user-facing strings.
- Avoid making `Step` a hidden execution language. `Step` should stay a
  user-facing render type; semantic work plans should own executable intent.
