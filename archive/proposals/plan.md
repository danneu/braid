# Plan: Migrate Braid to Config-Convergent `plan/apply`

## Status

Draft implementation plan for migrating from script-per-operation workflows
(`braid-add-disk`, `braid-remove-disk`, `braid-status`) to a unified
config-convergent workflow:

1. Edit `braid.disks`
2. `nixos-rebuild switch`
3. `braid plan`
4. `braid apply`

This plan assumes current behavior in:

- `scripts/braid-add-disk.sh`
- `scripts/braid-remove-disk.sh`
- `scripts/braid-status.sh`
- `docs/decisions/config-first-workflow.md`
- `docs/decisions/disk-pool-management.md`

## Why Change

Current tooling is safe and test-backed, but each operation is manually selected.
Users must decide which command to run (`add`, `remove`, replace-via-`add`) and
in what order. That is workable but not ideal for operator UX.

`plan/apply` improves this by:

- Making config/live drift explicit before mutation
- Providing one mental model for all mutations
- Enabling checkpointed/resumable execution
- Creating a stable machine interface for future daemon/TUI work

## Non-Negotiable Invariants

1. Nix config remains source of truth (`braid.disks` authoritative).
2. `nixos-rebuild switch` remains non-destructive.
3. Destructive or mutable storage operations happen only in explicit CLI actions.
4. Stable identifiers only (`/dev/disk/by-id/...`).
5. Boot resilience guarantees remain intact (degraded/nofail behavior unchanged).

## End-User UX (README Contract)

### Universal flow

```bash
# 1) edit braid.disks
sudo nixos-rebuild switch

# 2) preview what braid will do
sudo braid plan

# 3) execute the plan
sudo braid apply
```

### Add disk

```bash
# add new by-id path to braid.disks
sudo nixos-rebuild switch
sudo braid plan
sudo braid apply
```

### Remove healthy disk

```bash
# remove disk by-id path from braid.disks
sudo nixos-rebuild switch
sudo braid plan
sudo braid apply
```

### Replace dead disk

```bash
# remove dead by-id path, add replacement by-id path
sudo nixos-rebuild switch
sudo braid plan
sudo braid apply
```

### Status

```bash
sudo braid status
sudo braid status --verbose
```

Compatibility wrappers for old commands remain during migration.

## Command Model

Unified CLI entrypoint: `braid`.

- `braid plan`:
  - Read `/etc/braid/config.json`
  - Inspect live system (`btrfs`, `cryptsetup`, `lsblk`, mount state)
  - Produce deterministic action plan with warnings/confirmations
  - No mutation

- `braid apply`:
  - Execute current plan in strict order
  - Save checkpoints for resume after interruption/reboot
  - Exit non-zero on incomplete or unsafe state

- `braid status`:
  - Keep existing status behavior (summary + `--verbose`)
  - Add `--json` later

## Action Graph (Planned Internal Primitives)

Actions should be explicit, auditable, and resumable:

- `ADD_DISK_LUKS_FORMAT_OPEN`
- `ADD_DISK_BTRFS_ADD`
- `BALANCE_TO_RAID1`
- `REMOVE_DISK_GRACEFUL`
- `REMOVE_DISK_MISSING`
- `CLOSE_LUKS_MAPPER`
- `VERIFY_POOL_HEALTH`
- `VERIFY_EXPECTED_DISK_SET`

Each action records:

- preconditions
- command(s)
- success criteria
- rollback/recovery notes

## Edge-Case Handling

1. Reboot between rebuild and remove
- Expected: disk no longer auto-unlocks, pool mounts degraded.
- Plan should detect and choose:
  - graceful remove if target is open and mapped correctly
  - `remove missing` if target unavailable and pool reports missing

2. Multiple missing devices
- Refuse ambiguous `remove missing` unless plan can prove the intended target.
- Require explicit operator intervention if ambiguity remains.

3. Removing to one disk (loss of redundancy)
- Plan flags `redundancy_loss: true`.
- Apply requires escalated confirmation phrase:
  - `remove this disk without redundancy`

4. Disk present but wrong identity
- If mapper/device does not resolve to requested by-id target: refuse.

5. Interrupted apply
- Resume from checkpoint file.
- Completed actions are not re-run unless idempotence is guaranteed.

6. Pool unavailable/unmounted
- Plan fails with actionable diagnostics, unless scenario is a supported
  degraded/missing flow.

## Migration Strategy

### Phase 0: Lock docs and UX contract

- Promote this plan and README examples as the target workflow.
- Keep current script workflow documented as compatibility path until cutover.

### Phase 1: Implement read-only planner

- Add `braid plan` to compute config-vs-live diff.
- Output concise human summary and machine-stable structure (internal first).

### Phase 2: Implement apply engine

- Add checkpoint storage (e.g. `/var/lib/braid/apply-state.json`).
- Execute action graph using proven script logic.

### Phase 3: Introduce unified `braid` command UX

- Ship `braid status` subcommand equivalent to `braid-status`.
- Keep legacy scripts as wrappers:
  - `braid-add-disk` -> guided call into planner/apply path for add-only intent
  - `braid-remove-disk` -> guided call for remove-only intent
  - `braid-status` -> call `braid status`

### Phase 4: Replace workflow convergence

- Keep current replace behavior valid during migration.
- Then add first-class `braid replace-disk <old> <new>` as convenience on top of
  planner/apply primitives.

### Phase 5: JSON interfaces

- `braid plan --json`
- `braid status --json`

## Required Repo Updates

### Code

- Add unified CLI binary/entrypoint wiring in module packaging.
- Add planner and apply engine implementation (prefer Go daemon codebase reuse or
  dedicated CLI package; pick one and document rationale).
- Add checkpoint persistence path and schema.
- Keep existing shell scripts during transition; convert to wrappers later.

### Nix Module

- Update `modules/braid/cli.nix` to package `braid` command alongside compatibility
  commands.
- Ensure runtime dependencies remain explicit.

### Tests

Add VM tests for:

1. `braid plan` no-op when live matches config
2. plan shows add actions after config add + rebuild
3. plan shows graceful remove actions for healthy target
4. plan shows missing-device remove for degraded target
5. apply happy-path add/remove/replace
6. apply interrupted then resumed
7. redundancy-loss warning + escalated confirmation path
8. identity mismatch refusal
9. legacy command wrapper compatibility

### Docs

- Update `README.md` disk management to `plan/apply` first.
- Keep short compatibility note for legacy commands.
- Update `docs/1-user-stories.md` to include plan/apply examples.
- Add/update decision doc for unified CLI architecture and checkpoint model.
- Link this plan from relevant decision docs.

## Rollout and Compatibility

1. Introduce `braid plan` first (safe, read-only).
2. Ship `braid apply` behind clear “new workflow” docs.
3. Keep old commands working for at least one release cycle.
4. Deprecate old commands after parity + test coverage is proven.

## Risks and Mitigations

- Risk: Planner misclassification causes wrong action selection.
  - Mitigation: explicit action preconditions + VM coverage for edge cases.

- Risk: Resume logic re-runs unsafe steps.
  - Mitigation: checkpoint each step with strict success predicates.

- Risk: UX confusion during transition.
  - Mitigation: README-first guidance + wrapper hints + consistent wording.

## TODO Checklist

- [ ] Confirm this plan as the migration contract.
- [ ] Add decision doc for unified `braid` CLI (`plan/apply/status/replace`).
- [ ] Define plan output schema (human + machine representation).
- [ ] Define checkpoint file schema and retention policy.
- [ ] Implement `braid plan` read-only diff engine.
- [ ] Implement `braid apply` executor with resumable checkpoints.
- [ ] Add `braid status` subcommand parity with current `braid-status`.
- [ ] Wire unified CLI packaging in `modules/braid/cli.nix`.
- [ ] Keep legacy commands as compatibility wrappers.
- [ ] Add VM tests for plan no-op/add/remove/replace scenarios.
- [ ] Add VM tests for interrupted apply + resume.
- [ ] Add VM tests for degraded/missing ambiguity and identity mismatch refusal.
- [ ] Update `README.md` with `edit -> rebuild -> plan -> apply` workflow.
- [ ] Update `docs/1-user-stories.md` with new examples.
- [ ] Cross-link docs/decisions to this migration plan.
