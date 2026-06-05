# Simplify Crash-Paused Owed RAID1 Recovery

## Summary

Change `braid recover` to fail closed when owed RAID1 maintenance encounters a
non-idle or indeterminate btrfs balance state. Do not resume, cancel, remount, or
add a sidecar marker. Preserve `pending-op.json` and make the operator-facing
error explicit that automatic recovery is unsafe for crash-paused owed RAID1
maintenance.

This applies to all recover-owned owed RAID1 maintenance paths: add
post-balance, remove-missing post-maintenance, replace post-maintenance, and
generic live-pool RAID1 replay.

## Preflight

Verify the worktree carries no leftover state from the abandoned cancel/remount
approach before applying this plan. Both checks must pass:

- `git status --short --untracked-files=no` reports no tracked changes, so no
  old-plan state rides along into the implementation or the commit.
- `rg 'repro-btrfs-balance-cancel-idle|btrfs-balance-cancel-idle' flake.nix
  tests/repro` returns no hits -- the abandoned cancel-idle repro registration
  and its `tests/repro/btrfs-balance-cancel-idle.nix` import must both be absent
  (the registration would break `nix flake check`).

Only if a check fails: revert the offending `flake.nix` registration (`git
restore --staged --worktree flake.nix`) and remove any stray
`tests/repro/btrfs-balance-cancel-idle.nix`, then re-run both checks. Earlier
review rounds saw this registration staged; it has since been reverted, so the
gate currently passes with nothing to do.

## Key Changes

- In the owed RAID1 replay helper, replace the paused-balance resume branch with
  an idle-only gate:
  - `BalanceReport::Idle`: proceed with the existing soft RAID1 balance if the
    pool has at least two devices.
  - `BalanceReport::Paused`: return a hard recover error before any balance
    command.
  - `BalanceReport::Running` or `Unknown`: return a hard recover error before
    soft replay.
- Remove the unused automated resume primitive:
  - Delete `CmdRequest::BtrfsBalanceResume`, `pool_balance_resume`, and their
    unit tests.
  - Do not add `BtrfsBalanceCancel`, `pool_balance_cancel`,
    `relock_and_remount` usage, `raid1-replay-blocked`, or the cancel-idle repro
    test.
- Update dry-run preview for owed RAID1 maintenance:
  - Replace the `btrfs balance resume ...` placeholder with a status-check row:
    recover verifies the btrfs balance is idle before RAID1 replay.
  - Keep the soft RAID1 balance row, conditional on the runtime idle check and
    `>=2` devices.
- Update user-facing docs and internals. This is a broad-guarantee edit, not a
  "resume"-wording fix: every place that promises recovery finishes owed
  maintenance or always replays the soft RAID1 balance must carve out the
  crash-paused fail-closed case while preserving the idle/no-paused success path
  (resize, idle soft RAID1 balance, cleared journal). Authoritative homes to
  rewrite:
  - `README.md` Recovery section: the "finishes only the owed post-mutation
    maintenance, such as resize or soft RAID1 balance" guarantee. (Repo-root
    file -- the old stale-reference sweep scope omitted it.)
  - `docs/commands/recover.md`, multiple spots:
    - Summary sentence (line 5, "finishing owed maintenance, and clearing the
      pending-operation journal"): carve out the crash-paused fail-closed case --
      recover finishes owed maintenance and clears the journal only on the
      idle/no-paused path.
    - Step 9 (add `PostAddBalanceRaid1`, "finishes the owed RAID1 balance"):
      finishes it only when the balance is idle; fails closed otherwise.
    - Step 11 (replace/remove-missing post-maintenance): drop the "paused-balance
      resume" promise and carve out the owed soft RAID1 balance the same way.
    - The "it always prints a RAID1 soft-balance replay row pair before the final
      pending-op.json cleared line" output claim: make it conditional on the idle
      gate -- the fail-closed path prints no replay row and does not clear the
      journal.
    - Leave step 14 ("clears `pending-op.json` only after ... any owed balance
      work is done") as-is: a conditional invariant that already holds under
      fail-closed (paused = work not done = journal stays).
  - `docs/design/decisions/020-ups-integration.md` (Active): resolved-question 1
    ("no remaining single-profile chunks where RAID1 was intended, and a cleared
    pending-op.json", plus "a soft RAID1 balance is replayed for
    Add/RemoveMissing/Replace ... before the journal is cleared"), the deferred
    "balance still relies on remount/recover behavior" line, and the consequences
    "braid recover is load-bearing ... and VM tests prove that coverage" line.
    Narrow these to the crash-paused owed-RAID1 sub-case surfaced by the two
    retargeted VM tests (`ups-lb-during-remove-missing`,
    `ups-lb-during-balanced-add`); do not claim all four matrix tests change.
    Preserve the historical Draft->Active framing and the idle/no-paused success
    path; point to `balance-soft.md` for the underflow rationale.
  - `docs/design/decisions/017-runtime-disk-membership.md` (Active): the "braid
    recover is responsible for replaying or completing any owed post-mutation
    work before clearing the journal" and "finishes only the owed maintenance"
    statements.
  - `docs/design/principles.md` (Active, Principle 3 Safe-by-construction): the
    "`braid recover` is responsible for replaying or completing owed maintenance
    before clearing it" sentence. This is a clarification, not a reversal -- the
    "journal cleared only after the entire lifecycle succeeds" invariant already
    accommodates fail-closed (an unsafe crash-paused balance means the lifecycle
    did not succeed, so the journal stays). Make the fail-closed branch explicit:
    recover replays owed maintenance when the balance is idle, and fails closed
    preserving the journal when it is crash-paused.
  - `docs/design/decisions/018-systemd-lifecycle.md` (Active): step 4 of the
    `--systemd-stop` teardown narrative says "`braid recover` resumes the paused
    balance on the next boot" -- the direct statement of the removed behavior.
    Rewrite so next-boot recover fails closed on the persisted paused balance.
    Leave the teardown half intact (shutdown still pauses a running balance so
    the kernel persists it); only the resume-on-recover claim changes.
  - `docs/commands/add.md`: the `PostAddBalanceRaid1` line "resumes or runs the
    owed RAID1 balance, and clears `pending-op.json`". Keep the separate add
    preflight line ("refuses if a btrfs balance is *paused* ... resume or cancel
    it first") -- that is accurate manual operator advice.
  - `docs/guides/recovery-scenarios.md`, two spots: the add `PostAddBalanceRaid1`
    step "then finish the owed RAID1 balance", and the command-table row (line 14)
    that summarizes `braid recover` as "...rebuilds pool.json, clears journal" --
    note the journal clears on the idle/no-paused success path and is preserved
    when recover fails closed on a crash-paused balance.
  - `docs/guides/troubleshooting.md`: the `braid recover` fix section ends "It
    then clears the journal" unconditionally -- soften to the idle/no-paused
    success path, noting the crash-paused case preserves `pending-op.json` and
    asks for manual inspection.
  - `cli/src/main.rs` and `cli/src/recover.rs` output-mode doc-comments ("replay
    and paused-balance resume" / "The resume itself can be many minutes"): drop
    the paused-balance-resume rationale now that the resume primitive is gone.
  - Explain in `docs/internals/btrfs/balance-soft.md` that VM evidence showed
    crash-paused owed RAID1 balance replay can underflow btrfs block-group
    accounting, so braid preserves the journal instead of automating recovery.
  - Presentation (mdBook admonitions -- safety spots only): mdBook 0.5.2 (pinned
    via nixos-26.05) renders GitHub-style alerts natively, on by default, and
    `docs/book.toml` does not disable them. Use a callout for the two data-safety
    spots only:
    - `docs/internals/btrfs/balance-soft.md` underflow hazard -> `> [!WARNING]`
      (or `> [!CAUTION]`): replaying a crash-paused RAID1 balance can underflow
      btrfs block-group accounting and silently halve redundancy.
    - The fail-closed operator notice in `docs/commands/recover.md` and
      `docs/guides/troubleshooting.md` -> `> [!IMPORTANT]`: recover left
      `pending-op.json` in place; inspect btrfs manually before clearing recovery
      state.
    Everything else -- the ADR 020/017 carve-outs, principles.md, and the
    recovery-scenarios.md table cell and inline prose -- stays in the existing
    `**bold-label:**` idiom; do not scatter callouts into ADR narrative or table
    cells. This is the only place this plan introduces admonitions (the docs are
    callout-free today); a repo-wide convention change is out of scope.
  - Keep manual paused-balance advice in `status`, `doctor`, `unlock`, and
    mutating-command preflight docs.

## Test Plan

- Rust tests:
  - Owed RAID1 replay with `BalanceReport::Paused` fails, preserves the journal,
    and issues no resume or soft-balance command.
  - `Running` and `Unknown` also fail before soft replay, and -- like `Paused` --
    each asserts no resume/soft-balance command issued, no journal clear, and
    `pending-op.json` preserved. (The Summary promises every non-idle/
    indeterminate state preserves the journal; pin that per-state, not only for
    `Paused`.)
  - `Idle` still runs the existing soft RAID1 balance when
    `pool.devices.len() >= 2`.
  - `restore_raid1_after_commit=false` paths still skip balance probing/replay.
  - Dry-run preview no longer renders `btrfs balance resume`.
- VM tests:
  - Retarget `ups-lb-during-remove-missing` to expect fail-closed recovery:
    nonzero `braid recover`, `pending-op.json` preserved, no resume/cancel
    output, no forced-readonly/underflow in logs, and remaining `Data, single`
    chunks visible for manual reconciliation.
  - Update `ups-lb-during-balanced-add` so a paused-balance outcome expects
    fail-closed behavior; keep existing success assertions only for the
    idle/no-paused outcome.
- Verification:
  - `just test-rust`
  - `just check-output-ascii`
  - Build the book (`mdbook build docs`) and confirm the two new admonitions
    render as styled callouts -- not literal `[!WARNING]`/`[!IMPORTANT]` text --
    and that `mdbook-linkcheck2` still passes. (First admonitions in the repo;
    mdBook 0.5.2 renders them by default.)
  - `just test-vm ups-lb-during-remove-missing -rebuild`
  - `just test-vm ups-lb-during-balanced-add -rebuild`
  - Stale-reference sweep over `cli/src`, `docs`, `tests`, `modules`, and
    repo-root `README.md` (the old `cli/src docs tests modules` scope omitted
    README). Grep the broad guarantee phrasing, not just "resume": the "finish"
    stem near "owed" (finishes/finishing/finished + owed maintenance), "owed
    post-mutation maintenance", "always" + soft RAID1 balance replay,
    "no remaining single-profile chunks", "cleared pending-op.json", and "clears
    journal"/"clears the journal" near `recover` or `pending-op.json`. Also grep
    these exact known-stale strings as a tracked checklist: "recover resumes the
    paused balance" (ADR 018), "resumes or runs the owed RAID1 balance" (add.md),
    "finish the owed RAID1 balance" (recovery-scenarios.md step), "finishing owed
    maintenance" (recover.md summary), "finishes the owed RAID1 balance"
    (recover.md step 9), "It then clears the journal" (troubleshooting.md), "clears
    journal" (recovery-scenarios.md table row), and "replay and paused-balance
    resume" (main.rs, recover.rs).
  - The "clears the journal" phrase is common and mostly legitimate. Flag only
    unconditional "recover finishes owed maintenance / then clears the journal"
    promises that omit the crash-paused fail-closed carve-out. Allow and do NOT
    edit: normal non-interrupted command flows (e.g. `add.md` "balances data to
    RAID1, then clears the journal"), the PoolMutation-not-committed recover path
    (`recover.md` step 10, which has no owed maintenance to fail on), identity
    statements ("`braid recover` is the only command that clears the journal" in
    ADR 017 and principles.md), the ~recover.rs internal comments/tests updated by
    the helper rewrite, manual operator advice, the new internals bug explanation,
    and idle/no-paused success-path examples.

## Assumptions

- Start from clean `HEAD` for the old cancel/remount implementation, enforced by
  the Preflight gate above. Earlier rounds saw a staged `flake.nix` cancel-idle
  registration; it has since been reverted and the repro `.nix` it imported never
  existed, so the gate currently passes with nothing to remove.
- No manual reconciliation command is added in this change. The safe v1 behavior
  is to preserve `pending-op.json` and tell the operator that manual btrfs
  inspection is required before clearing recovery state.
