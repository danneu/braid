# Disk Migration System Plan

## Status

Draft. This document defines the migration from script-per-operation workflows to
a unified config-convergent workflow based on `braid plan` and `braid apply`.

## Context

Current system behavior is implemented and tested via:

- `scripts/braid-add-disk.sh`
- `scripts/braid-remove-disk.sh`
- `scripts/braid-status.sh`
- `docs/decisions/config-first-workflow.md`
- `docs/decisions/disk-pool-management.md`

Current operator flow is safe but split across multiple commands. The new system
should unify mutation workflows while preserving all existing invariants.

## Goals

1. One operator mental model for add/remove/replace:
   `edit config -> rebuild -> plan -> apply`
2. Deterministic dry-run before mutation.
3. Checkpointed/resumable apply.
4. Backward compatibility during migration.

## Non-Negotiable Invariants

1. `braid.disks` remains authoritative.
2. `nixos-rebuild switch` remains non-destructive.
3. Mutable storage operations happen only in explicit CLI steps.
4. Stable identifiers only (`/dev/disk/by-id/...`).
5. Resilient degraded boot behavior remains unchanged.

## Implementation Choice

Phases 1-3 use **bash + jq** (not Go).

Rationale:

- Reuses existing tested scripts and operational patterns.
- Lowest migration risk and fastest delivery.
- JSON plan/checkpoint formats are fully feasible with jq.

Re-evaluate language choice only after plan/apply semantics are stable and daemon
scope is concrete.

## End-User UX Contract (README-First)

### Universal flow

```bash
# 1) edit braid.disks
sudo nixos-rebuild switch

# 2) preview actions
sudo braid plan

# 3) execute convergence
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
# remove by-id path from braid.disks
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
sudo braid status --json
```

## Command Set

- `braid plan` (read-only)
- `braid plan --json`
- `braid apply`
- `braid apply --resume`
- `braid status`
- `braid status --verbose`
- `braid status --json`

Compatibility commands remain during transition:

- `braid-add-disk`
- `braid-remove-disk`
- `braid-status`

## Live State Discovery (Planner Core)

`braid plan` computes desired-vs-live diff using:

1. Desired state:
   - `/etc/braid/config.json` (`disks`, `mountPoint`)
2. Canonical identity mapping:
   - resolve each `/dev/disk/by-id/*` with `readlink -f`
3. Open LUKS mappers:
   - `cryptsetup status <mapper>`
   - `lsblk` to map dm devices to underlying block devices
4. Pool membership and missing devices:
   - `btrfs filesystem show <mountPoint>`
5. Mount/profile/capacity:
   - `findmnt`
   - `btrfs filesystem df` (or usage command used by existing scripts)

Planner output must fail closed on ambiguity (for example, multiple missing
devices when target cannot be proven).

## Action Model

Actions are explicit and ordered:

- `ADD_DISK_LUKS_FORMAT_OPEN`
- `ADD_DISK_BTRFS_ADD`
- `BALANCE_TO_RAID1`
- `REMOVE_DISK_GRACEFUL`
- `REMOVE_DISK_MISSING`
- `CLOSE_LUKS_MAPPER`
- `VERIFY_POOL_HEALTH`
- `VERIFY_EXPECTED_DISK_SET`

Action fields:

- `id`
- `type`
- `target` (by-id path or mapper or logical target such as `missing`)
- `preconditions`
- `warnings`
- `confirmation` (if required)
- `status`

## `braid plan` Output Format

### Human output (mock)

```text
Plan ID: 2026-02-20T18:35:12Z-7f13a2
Mount: /mnt/storage
Actions: 3

[1] ADD_DISK_LUKS_FORMAT_OPEN  target=/dev/disk/by-id/ata-WDC_NEW
[2] ADD_DISK_BTRFS_ADD         target=/dev/mapper/ata-WDC_NEW
[3] BALANCE_TO_RAID1           target=/mnt/storage

Warnings: none
Confirmations required: 1
Next step: run 'sudo braid apply'
```

### JSON output (v1 shape)

```json
{
  "schema_version": 1,
  "plan_id": "2026-02-20T18:35:12Z-7f13a2",
  "mount_point": "/mnt/storage",
  "warnings": [],
  "confirmations": [
    {
      "action_id": "a1",
      "phrase": "remove this disk without redundancy"
    }
  ],
  "actions": [
    {
      "id": "a1",
      "type": "REMOVE_DISK_GRACEFUL",
      "target": "/dev/mapper/ata-OLD",
      "preconditions": ["target_mapper_open", "target_in_pool"],
      "status": "pending"
    }
  ]
}
```

## Checkpoint Schema and Retention

Active checkpoint file: `/var/lib/braid/apply-state.json`

### Checkpoint JSON (v1 shape)

```json
{
  "schema_version": 1,
  "plan_id": "2026-02-20T18:35:12Z-7f13a2",
  "created_at": "2026-02-20T18:35:12Z",
  "updated_at": "2026-02-20T18:36:01Z",
  "config_hash": "sha256:...",
  "live_state_hash": "sha256:...",
  "last_completed_action_id": "a2",
  "actions": [
    {
      "id": "a1",
      "type": "ADD_DISK_LUKS_FORMAT_OPEN",
      "target": "/dev/disk/by-id/ata-NEW",
      "status": "completed",
      "started_at": "2026-02-20T18:35:20Z",
      "completed_at": "2026-02-20T18:35:40Z",
      "error": ""
    },
    {
      "id": "a2",
      "type": "ADD_DISK_BTRFS_ADD",
      "target": "/dev/mapper/ata-NEW",
      "status": "pending",
      "started_at": "",
      "completed_at": "",
      "error": ""
    }
  ]
}
```

Allowed status values:

- `pending`
- `in_progress`
- `completed`
- `failed`
- `skipped`

Retention policy:

1. On success: remove active checkpoint file.
2. Write final execution summary to `/var/lib/braid/history/<plan_id>.json`.
3. Keep last `N` history files (default 20), prune oldest.
4. On failure: keep active checkpoint for `braid apply --resume`.

## Edge Case Handling

1. Reboot between rebuild and remove:
   - planner picks graceful remove only if target mapper is open and identity is proven
   - otherwise uses missing-remove path if supported by pool state
2. Multiple missing devices:
   - refuse ambiguous missing-remove
3. Remove to single-disk (redundancy loss):
   - allow only with explicit phrase:
     `remove this disk without redundancy`
4. Present-but-wrong identity:
   - hard fail
5. Interrupted apply:
   - resume from checkpoint
   - if checkpoint `config_hash` or `live_state_hash` no longer matches current
     state, refuse resume and require `braid plan` before next apply
6. Unmounted/unavailable pool:
   - fail with actionable diagnostics except supported degraded/missing flow

## Phased Migration

### Phase 0: UX and docs lock

- Adopt this plan as contract.
- Document universal flow in README draft before implementation.

### Phase 1: Planner

- Implement `braid plan` + `braid plan --json`.
- No mutation.

### Phase 2: Apply engine

- Implement `braid apply` + `braid apply --resume`.
- Checkpoint persistence and resume semantics.

### Phase 3: Unified status and wrappers

- Implement `braid status` + `--verbose` + `--json`.
- Keep legacy commands as wrappers to unified command path.

Note: A dedicated `braid replace-disk` command is optional future UX sugar, not a
required migration phase.

## Required Updates

### Code

- Add unified `braid` CLI script/binary packaging.
- Add planner engine in bash+jq.
- Add apply engine with checkpoint writes and resume.
- Preserve and wrap existing scripts during transition.

### Module packaging

- Update `modules/braid/cli.nix` to install unified `braid` command and wrappers.
- Keep explicit runtime inputs (`cryptsetup`, `btrfs-progs`, `util-linux`, `jq`).

### Tests

Add/extend VM tests for:

1. `braid plan` no-op
2. planner add/remove/replace diffs
3. planner ambiguity refusal (multiple missing)
4. apply happy paths
5. apply interruption + resume
6. redundancy-loss confirmation path
7. identity mismatch refusal
8. wrapper compatibility behavior
9. `plan --json` and `status --json` schema assertions

### Docs

- Update `README.md` to universal `plan/apply` workflow.
- Update `docs/1-user-stories.md` examples.
- Add decision doc for unified CLI + checkpoint model.
- Cross-link relevant decision docs to this plan.

## Risks and Mitigations

- Risk: wrong plan classification
  - Mitigation: fail-closed ambiguity handling + VM coverage
- Risk: unsafe resume behavior
  - Mitigation: strict checkpoint state machine and success predicates
- Risk: transition confusion
  - Mitigation: wrapper guidance and README-first rollout

## TODO Checklist

- [ ] Adopt this document as migration contract.
- [ ] Add decision doc for unified `braid` CLI architecture.
- [ ] Implement `braid plan` (human + `--json`).
- [ ] Implement deterministic live-state discovery logic.
- [ ] Implement `braid apply` with checkpoint persistence.
- [ ] Implement `braid apply --resume`.
- [ ] Implement `braid status` + `--verbose` + `--json`.
- [ ] Update `modules/braid/cli.nix` to package unified command + wrappers.
- [ ] Add planner/apply VM tests (including ambiguity, identity mismatch).
- [ ] Add resume VM test with interrupted apply.
- [ ] Add JSON schema assertions for plan/status outputs.
- [ ] Update `README.md` disk management to `edit -> rebuild -> plan -> apply`.
- [ ] Update `docs/1-user-stories.md` with new flow.
- [ ] Add doc links from decision docs to this migration plan.
- [ ] Define history retention `N` in config or constant and test pruning.
