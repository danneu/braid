# Phased Journals for Replace and Remove-Missing

## Summary

Extend the phased journal model from existing-pool `add` to `replace` and
`remove-missing`, while keeping the semantics command-specific.

The shared invariant is: once btrfs membership has committed and `pool.json`
has been durably saved, the journal phase must advance to a post-mutation
maintenance phase. Recovery in that post phase must never rerun the primary
btrfs membership mutation.

Also fix one Add implementation issue found during planning: Add PoolMutation
recovery must resolve and verify any needed passphrase before acquiring the
sleep inhibitor, so prompts and reversible credential checks stay outside the
inhibitor window.

## Why phased journals over live-state inference

A simpler alternative was considered and rejected: keep all the enriched journal
payloads this plan adds (the `new_target` and `source` fields on
`OpKind::Replace`, the `restore_raid1_after_commit: bool` on both
`OpKind::Replace` and `OpKind::RemoveMissing`), but omit the phase enum.
Recovery would infer commit status by comparing live membership to
`pre_membership` / `target_membership` rather than dispatching on a journaled
phase. The single dimension that varies between the two designs is whether
commit status is encoded in the journal (phasing) or derived from live state
(inference). The recovery handlers in this plan already perform live-state
probing inside their `PoolMutation` arms, so the inference path is workable for
both commands.

Phased journals are preferred for the following reasons:

- **Consistency with existing Add phasing.** `Add` already uses
  `AddPhase::{PoolMutation, PostAddBalanceRaid1}` (cli/src/journal.rs). Matching
  that shape for `Replace` and `RemoveMissing` keeps recover.rs's dispatch
  uniform -- every multi-step op recovers via the same phase-arm pattern. The
  alternative would leave Add phased and the other two relying on inference,
  forcing two recovery patterns to coexist.

- **Structural safety guarantee, not inference guarantee.** The invariant
  "recovery in a post phase must never rerun the primary btrfs membership
  mutation" becomes a property of the dispatch table -- the post-phase arm
  literally does not call `btrfs replace start` / `btrfs device remove`. Under
  inference, the same invariant relies on the handler classifying live state
  correctly in every edge case, including states neither cleanly pre nor cleanly
  post.

- **Post-`pool.json` commitment record.** For `Replace`, several
  `target_membership` fields (notably the new disk's `devid`) are only
  populated after `btrfs replace start -B` succeeds. Once `pool.json` is durably
  saved with the enriched target, the command performs a separate `atomic_write`
  to rewrite the journal to the post-mutation phase. The `pool.json` save and
  the journal rewrite are sequential `atomic_write` calls with a crash window
  between them; if a crash lands inside that window, `PoolMutation` recovery
  closes the gap by recomputing the committed target via
  `build_membership_from_live_pool` (cli/src/recover.rs:1105) and advancing the
  phase itself. The benefit of the journal rewrite is the common-case shortcut:
  when it lands before any crash, post-phase recovery starts from a
  known-committed state and skips the commit-detection classifier entirely.

- **Less inference code in recovery handlers.** Post-phase handlers can assume
  the primary mutation has committed and skip the "did it commit?" classifier.
  The PoolMutation arm still infers, but it is the only arm that needs to.

- **Future-proofing.** If a command later grows additional post-mutation phases
  (e.g., a separate resize phase before balance for `Replace`), extending the
  phase enum is additive. The inference-only approach would need new flag fields
  with overlapping semantics.

Trade-off: phasing requires one extra durable journal rewrite per command,
sequenced after the `pool.json` save. It is a separate `atomic_write` (not
bundled with the `pool.json` write), so there is a crash window between the two
-- closed by `PoolMutation` recovery's existing live-state inference path. The
judgment is that this modest extra write plus a single inference site
(PoolMutation) is preferable to making inference the only commit-detection
mechanism everywhere.

## Key Changes

- Add durable phase-transition support in the journal layer by reusing the
  existing full-file atomic write path. Prefer a small helper that clones the
  current `Journal`, replaces `op`, optionally replaces `target_membership`,
  and calls `write_journal`.
- Make recover dispatch exhaustive over every `(op, phase)` pair. Do not leave
  a catch-all path that can route a phased op through generic recovery.
- Scope the old generic `replay_post_mutation` helper to Add balance replay
  plus Remove's explicit no-op. Replace and RemoveMissing post-maintenance must
  be owned only by their phase-specific handlers.
- Extend `OpKind::RemoveMissing`:

  ```rust
  enum RemoveMissingPhase {
      PoolMutation,
      PostRemoveMissingMaintenance,
  }

  RemoveMissing {
      phase: RemoveMissingPhase,
      devid: u64,
      restore_raid1_after_commit: bool,
  }
  ```

  Compute `restore_raid1_after_commit` as
  `pre_missing_count == 1 && pre_present_count >= 2`, where
  `pre_present_count` is the pre-op live `pool.devices.len()`.

- Extend `OpKind::Replace`:

  ```rust
  enum ReplacePhase {
      PoolMutation,
      PostReplaceMaintenance,
  }

  enum ReplaceJournalSource {
      Live { old_devid: u64, old_mapper: MapperName },
      Missing { old_devid: u64 },
  }

  struct ReplaceJournalTarget {
      by_id: ByIdPath,
      mapper_name: String,
      mode: ReplaceJournalMode,
  }

  enum ReplaceJournalMode {
      FreshLuks {
          luks_label: String,
          luks_format_extra_opts: Vec<String>,
          enroll_key_file: Option<PathBuf>,
      },
      ExistingLuks {
          luks_uuid: LuksUuid,
      },
  }

  Replace {
      phase: ReplacePhase,
      old_name: String,
      new_name: String,
      new_by_id: ByIdPath,
      new_target: ReplaceJournalTarget,
      source: ReplaceJournalSource,
      restore_raid1_after_commit: bool,
  }
  ```

  Compute `restore_raid1_after_commit` as
  `source is Missing && pre_missing_count == 1 && pre_present_count + 1 >= 2`,
  where `pre_present_count` is the pre-op live `pool.devices.len()`.
  Compute `new_target` from the pre-journal new-disk probe: fresh disks store
  the exact effective LUKS format options, generated `--label braid-<new>`, and
  optional keyfile path; existing LUKS disks store the observed LUKS UUID.

- No backward-compatible journal migration is needed. Update all journal
  construction, tests, and injected fixture JSON to the new shapes.

## Command Behavior

- `remove-missing` writes `phase: PoolMutation` before
  `btrfs device remove <devid>`.
- After `btrfs device remove <devid>` succeeds, `remove-missing` saves
  `pool.json`, atomically rewrites the journal to
  `PostRemoveMissingMaintenance`, then runs the owed RAID1 restore only when
  `restore_raid1_after_commit` is true. Clear the journal only after
  maintenance completes.
- `replace` writes `phase: PoolMutation` before new-disk LUKS setup or
  `btrfs replace start`.
- After `btrfs replace start -B` succeeds, `replace` probes/enriches target
  membership, saves that enriched membership to `pool.json`, and atomically
  rewrites the journal to `PostReplaceMaintenance` with the same enriched
  `target_membership`. Then it runs best-effort old mapper close for
  live-source replaces, resize-to-max on the new mapper's live devid, and the
  owed RAID1 restore only when `restore_raid1_after_commit` is true. Clear the
  journal only after maintenance completes.
- Keep the existing command-side sleep inhibitor boundary: acquire before the
  initial journal write and hold through phase advance, maintenance, and
  journal clear.

## Recovery Behavior

- Mount membership selection:
  - `Add::PoolMutation`: keep existing Add behavior, mount from
    `pre_membership`.
  - `Add::PostAddBalanceRaid1`: mount from `target_membership`.
  - `RemoveMissing::PoolMutation`: mount from `pre_membership`.
  - `RemoveMissing::PostRemoveMissingMaintenance`: mount from
    `target_membership`.
  - `Replace::PoolMutation`: keep union membership and the existing replace
    relock/remount safety path.
  - `Replace::PostReplaceMaintenance`: mount from `target_membership`.
- Replace recovery mount-cycle behavior:
  - The already-mounted Replace refusal in `plan_recover` applies only to
    `Replace { phase: PoolMutation, .. }`.
  - The `wait_for_kernel_replace_to_finish` plus `relock_and_remount` cycle
    applies only to `Replace { phase: PoolMutation, .. }` after a fresh recover
    mount.
  - `Replace::PostReplaceMaintenance` skips both the kernel dev_replace wait
    and the relock/remount cycle, because replace membership has already
    committed and `pool.json` was durably saved before this phase was written.

- `RemoveMissing::PoolMutation` recovery:
  - Probe live btrfs state.
  - If the journaled `devid` is gone from `missing_devids`, treat membership as
    committed: write recovered target membership, advance to
    `PostRemoveMissingMaintenance`, then run post maintenance.
  - If the journaled `devid` is still present in `missing_devids`, treat the
    primary mutation as not committed: keep or restore `pre_membership`, clear
    the journal, and print guidance to rerun `braid remove-missing`.
  - If live state is neither safely pre nor safely target, fail and preserve
    the journal.
  - Never rerun `btrfs device remove` from recovery.

- `RemoveMissing::PostRemoveMissingMaintenance` recovery:
  - Validate the live pool no longer contains the journaled missing `devid`.
  - Repair stale or missing `pool.json` only when live state matches
    `target_membership`.
  - Acquire a sleep inhibitor before any balance work.
  - If `restore_raid1_after_commit` is true, resume a paused balance if
    present, then run the RAID1 soft balance. If false, do not resume or run
    balance.
  - Clear the journal only after owed maintenance succeeds.

- `Replace::PoolMutation` recovery:
  - Keep the existing refusal for already-mounted replace recovery in this
    phase, because an interrupted dev_replace may require a clean
    relock/remount cycle.
  - After mount/relock, probe live btrfs state.
  - If the live pool has `new_name` and no longer has `old_name`, treat replace
    as committed: write recovered target membership, advance to
    `PostReplaceMaintenance` with that recovered membership as the journal's
    `target_membership`, then run post maintenance.
  - If the live pool still matches the pre-replace topology, treat replace as
    not committed. Before clearing the journal, reconcile any journaled
    new-target prep:
    - `ExistingLuks`: no header backup or keyfile enrollment was owed; keep or
      restore `pre_membership`, clear the journal, and print guidance to rerun
      `braid replace`.
    - `FreshLuks` with the by-id path still non-LUKS: no fresh prep committed;
      keep or restore `pre_membership`, clear the journal, and print guidance
      to rerun `braid replace`.
    - `FreshLuks` with a LUKS header carrying the expected label: do not
      reformat; resolve and verify the credential against existing pool members
      and the new LUKS target itself, ensure requested keyfile enrollment
      idempotently, run the LUKS header backup byproduct, then keep or restore
      `pre_membership`, clear the journal, and print guidance to rerun
      `braid replace`. Resolve and verify credentials before acquiring the
      sleep inhibitor; acquire the inhibitor before keyfile enrollment or
      header backup and hold it through pre-membership restore and journal
      clear.
    - `FreshLuks` with a missing target or an unexpected LUKS identity: fail
      and preserve the journal.
    - `FreshLuks` with a credential rejection, keyfile probe/enrollment error,
      or header-backup failure: fail and preserve the journal.
  - If live state is mixed or unexpected, fail and preserve the journal.
  - Never rerun `btrfs replace start` from recovery.

- `Replace::PostReplaceMaintenance` recovery:
  - Allow already-mounted recovery, because dev_replace has already committed
    in this phase.
  - If recovery must mount the pool, use `target_membership` and do not run the
    replace-specific kernel dev_replace wait or relock/remount cycle.
  - Validate live membership exactly matches `target_membership`.
  - Repair stale or missing `pool.json` only from exact committed target state.
  - Acquire a sleep inhibitor before resize or balance work.
  - For live-source replace, best-effort close the old mapper if it still
    exists; warn and continue on close failure.
  - Resize the new mapper's live devid to max. Failure preserves the journal.
  - If `restore_raid1_after_commit` is true, resume paused balance if present,
    then run RAID1 soft balance. If false, skip balance resume/replay.
  - Clear the journal last.

- Add fix:
  - In `execute_add_pool_mutation_recovery`, reorder the second
    `if !add_targets_all_live(&pool, targets)` branch so
    `recover_passphrase(...)` runs before inhibitor acquisition.
  - Run `verify_recover_passphrase_for_add_replay(...)` before inhibitor
    acquisition. Prompts, passphrase reads, and reversible credential checks
    must stay outside the inhibitor window.
  - Acquire `sleep_inhibitor.acquire("replaying interrupted add")` only after
    credential verification succeeds, immediately before destructive replay.
  - Keep the guard alive through the replay loop, `pool.json` save, phase
    rewrite, and the immediate
    `execute_add_post_balance_recovery(..., true)` path.
  - Preserve the existing rule that dry-run never acquires an inhibitor.

## Test Plan

- Journal tests:
  - Round-trip all new phase enums and op variants.
  - Verify Add and RemoveMissing phase rewrites preserve `started_at`,
    `pre_membership`, and `target_membership`.
  - Verify Replace phase advancement preserves `started_at` and
    `pre_membership` but writes the enriched committed `target_membership`.
  - Verify Replace journal construction records fresh target prep intent,
    including effective format options, generated label, and keyfile path.
  - Verify Replace journal construction records existing-LUKS target identity
    with the observed LUKS UUID.
  - Update all injected journal JSON in unit and VM tests.
  - Add a compile-enforced exhaustive recover dispatch table over every
    `OpKind` / phase shape; no catch-all may route phased Replace or
    RemoveMissing through generic recovery.

- Command ordering tests:
  - `remove-missing`: after device-remove success and `pool.json` save, balance
    failure leaves a `PostRemoveMissingMaintenance` journal.
  - `replace`: after replace success and `pool.json` save, resize or balance
    failure leaves a `PostReplaceMaintenance` journal.
  - Neither command clears the journal before owed maintenance completes.

- Recovery tests:
  - `RemoveMissing::PoolMutation` with target committed advances phase and
    finishes maintenance without running `btrfs device remove`.
  - `RemoveMissing::PoolMutation` with target not committed restores pre state,
    clears the journal, and tells the user to rerun.
  - `RemoveMissing::PoolMutation` with live membership that is neither exact
    pre nor exact target fails, preserves the journal, does not advance phase,
    and runs no post-maintenance.
  - `RemoveMissing::PostRemoveMissingMaintenance` repairs stale `pool.json`,
    runs balance only when owed, and never runs device remove.
  - `Replace::PoolMutation` with target committed advances phase and finishes
    resize/balance without running `btrfs replace start`; the post-phase
    journal contains the enriched committed `target_membership`.
  - `Replace::PoolMutation` with target not committed restores pre state,
    clears the journal, and tells the user to rerun.
  - `Replace::PoolMutation` with live membership that is neither exact pre nor
    exact target fails, preserves the journal, does not advance phase, and runs
    no post-maintenance.
  - `Replace::PoolMutation` with pre topology and a fresh target already
    formatted with the expected label does not reformat, ensures requested
    keyfile enrollment, runs header backup, clears the journal, and tells the
    user to rerun.
  - `Replace::PoolMutation` with pre topology and a fresh expected-label target
    resolves/verifies credentials before acquiring the inhibitor; if inhibitor
    acquisition fails, it runs no keyfile enrollment, header backup,
    `pool.json` restore, journal clear, or `btrfs replace start`.
  - `Replace::PoolMutation` with pre topology and a fresh target with wrong
    LUKS identity fails before keyfile enrollment, header backup, journal clear,
    or `btrfs replace start`.
  - `Replace::PoolMutation` with pre topology and `FreshLuks` failure states:
    `ConfigDiskState::Absent` and credential rejection both fail while
    preserving the journal. Assert no keyfile enrollment, no header backup, no
    `pool.json` restore, no journal clear, no `btrfs replace start`; for
    credential rejection, also assert no inhibitor acquisition.
  - `Replace::PostReplaceMaintenance` allows already-mounted recovery,
    validates exact target membership, resizes the new device, and never
    formats, opens as target prep, or starts replace.
  - `Replace::PostReplaceMaintenance` when not already mounted mounts from
    `target_membership`, skips `wait_for_kernel_replace_to_finish`, skips the
    relock/remount cycle, runs resize, and clears the journal.
  - `Replace::PostReplaceMaintenance` for live-source replace with
    `restore_raid1_after_commit=false` and a paused-balance mock resizes the
    new device, does not run balance resume or RAID1 soft balance, and clears
    the journal.
  - `Replace::PostReplaceMaintenance` already-mounted recovery is not rejected
    by the replace-specific PoolMutation guard.
  - Inhibitor acquisition failure in any post-maintenance phase runs no
    maintenance command and preserves the journal.
  - Add regression: Add PoolMutation recovery with a bad credential uses
    `RecordingInhibitor` and fails with acquire count `0`.
  - Add regression: Add PoolMutation recovery with a good credential records
    command order and proves every `CryptsetupTestPassphrase` verification
    occurs before the first sleep-inhibitor acquisition.
  - Add regression: the Add PoolMutation failing-inhibitor replay test provides
    successful credential verification mocks, proving inhibitor failure is
    reached only after credential verification succeeds and still before
    destructive replay.

- Existing VM/repro tests:
  - Update injected and inspected `pending-op.json` payloads to the new phased
    shapes, including `tests/cli/recover-replace-completed.py` and
    `tests/cli/recover-replace-not-started.py`.
  - Add one VM test that injects a `RemoveMissing::PoolMutation` journal while
    the live pool is already in the post-commit topology: the journaled missing
    devid is gone from `missing_devids`. Assert recovery mounts using the right
    membership, advances to post maintenance, runs the gated soft balance, and
    clears the journal.
  - Keep crash-window coverage mostly unit-level; VM tests should prove
    end-to-end recovery still clears the journal and preserves data.

## Assumptions

- Scope is limited to `add`, `replace`, and `remove-missing`. Do not phase
  `remove` in this rollout.
- Recovery for `replace` and `remove-missing` does not restart the primary
  btrfs mutation. If the primary mutation definitely did not commit, recovery
  exits recovery mode and tells the operator to rerun the original command.
- `restore_raid1_after_commit` is the only trigger for recover-side balance
  resume/replay in replace and remove-missing post phases.
- Documentation updates are required in `docs/principles.md`,
  `docs/decisions/012-intent-cli.md`,
  `docs/decisions/017-runtime-disk-membership.md`,
  `docs/decisions/019-inhibit-sleep.md`, and the README recovery section.
