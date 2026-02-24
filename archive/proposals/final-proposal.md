# Final Proposal: Safe-by-Construction Disk Lifecycle for `braid`

**Status:** Draft  
**Scope:** Unified CLI behavior (`braid plan`, `braid apply`, `braid init-disk`) and compatibility handling for legacy standalone scripts.

## 1. Problem Statement

`braid apply` currently mixes two fundamentally different operation classes:

1. One-time destructive initialization (`cryptsetup luksFormat`)
2. Repeatable reconciliation (`luksOpen`, `btrfs device add/remove`, balance, verify)

This creates a structural hazard: state ambiguity (temporarily absent disk vs truly new disk) can route execution toward formatting.

## 2. Non-Negotiable Design Goals

1. `braid plan` is read-only.
2. `braid apply` is safe to run repeatedly; it never performs destructive disk initialization.
3. Disk formatting is explicit, one-shot, and never inferred.
4. Missing devices are tolerated in fresh reconciliation runs (resilience-first), but checkpointed in-flight actions remain strict.
5. Declarative config remains end-state only; no imperative lifecycle flags in NixOS config.
6. Truth comes from config + on-disk metadata + live state. No mandatory hidden registry.

## 3. Final Architecture

### 3.1 Command Responsibilities

1. `braid init-disk <by-id>`: destructive initializer, explicit operator intent.
2. `braid plan`: compute read-only reconciliation plan from current config + live state.
3. `braid apply`: execute reconciliation actions only, never format.
4. `braid status`: reporting only.

### 3.2 Hard Boundary

`cryptsetup luksFormat` is forbidden in `plan` and `apply` code paths.

Only `init-disk` may invoke `luksFormat`.

## 4. Detailed Command Contracts

### 4.1 `braid init-disk`

**Synopsis**

```bash
braid init-disk /dev/disk/by-id/<disk-id> [--force] [--config <path>]
```

**Behavior (must happen in this order):**

1. Validate `by-id` path exists and resolves to a block device.
2. Validate disk is declared in `config.disks` (preserve config-first workflow).
3. Validate target is not currently part of mounted pool membership.
4. Probe LUKS header (`cryptsetup isLuks`):
   - If LUKS and no `--force`: fail with non-zero and remediation.
   - If LUKS and `--force`: continue only after explicit confirmation phrase.
5. Require passphrase input (`BRAID_PASSPHRASE` initially; interactive prompt may be added later).
6. Enforce passphrase invariant (single-passphrase principle) when pool already has members:
   - verify entered passphrase can unlock an existing pool member (`cryptsetup --test-passphrase` equivalent check).
   - fail if mismatch.
7. Run `cryptsetup luksFormat` with project-standard options.
8. Exit success with clear next step: run `braid apply`.

**`--force` policy:**

1. Requires both:
   - target not in active pool
   - `BRAID_CONFIRM='reformat this disk'`
2. If either check fails: refuse.

**Postcondition:**

Disk is LUKS formatted; device is not required to be opened or added to pool by `init-disk`.

### 4.2 `braid plan`

**Invariant:** never mutates system state.

**Synopsis**

```bash
braid plan [--json] [--allow-remove-missing] [--config <path>]
```

**Planner behavior for config disk not in pool:**

1. Device present and LUKS-openable: plan `OPEN_LUKS` + `ADD_DISK_BTRFS_ADD`.
2. Device present but not LUKS: do not emit action; emit blocking error annotation with remediation to run `braid init-disk`.
3. Device absent: emit warning and skip add actions for that disk in this plan.

**Missing pool members:**

1. No inferred destructive `REMOVE_DISK_MISSING` by default.
2. Planner may include non-destructive warnings about degraded pool.
3. Removal of missing devices happens only via explicit remove workflow (see section 4.4).

**Output additions:**

1. `warnings[]` must include machine-parseable codes + human messages.
2. `blocked_reasons[]` for conditions that make apply non-applicable.
3. Plan can be:
   - `applicable`
   - `blocked`

### 4.3 `braid apply`

**Invariant:** no formatting operations.

**Synopsis**

```bash
braid apply [--resume] [--allow-remove-missing] [--config <path>]
```

**Fresh apply flow:**

1. Discover live state.
2. Compute plan inline with the same planner gates as `braid plan` (including `--allow-remove-missing` when provided).
3. If plan is `blocked`, refuse and print actionable remediation.
4. If plan has no mutation actions, exit success with no-op message.
5. Persist checkpoint and execute actions.

**Resume flow (`--resume`):**

1. Require existing checkpoint.
2. Verify config hash unchanged (existing behavior).
3. Re-validate runtime preconditions before each pending action.
4. If a previously targeted disk for pending action is now absent, fail resume at that action (do not silently skip).

**Action precondition enforcement:**

Preconditions are runtime-gated, not informational only. Any precondition failure aborts apply and preserves checkpoint.

### 4.4 Missing Disk Policy (Converged Decision)

#### Fresh apply (newly computed plan)

1. If configured disk absent: `skip + warning`.
2. Continue unrelated reconciliation on available devices.
3. Never substitute with destructive fallback.

#### Resume apply (checkpoint replay)

1. If next pending action targets missing disk: fail that action and stop.
2. Checkpoint remains for operator intervention.

#### Explicit missing-device removal

1. Missing-device eviction (`btrfs device remove missing`) must require explicit operator intent.
2. Required gate:
   - operator passes `braid apply --allow-remove-missing`
   - and sets `BRAID_CONFIRM='remove missing device from pool'`
3. Planner may only emit `REMOVE_DISK_MISSING_EXPLICIT` when that gate is present.
4. Without the gate, planner emits warning only; no missing-removal action is produced.
5. Never infer missing-device removal solely from transient absence.

### 4.5 Action Type Changes

**Remove:**

1. `ADD_DISK_LUKS_FORMAT_OPEN`

**Add/retain:**

1. `OPEN_LUKS`
2. `ADD_DISK_BTRFS_ADD`
3. `REMOVE_DISK_GRACEFUL`
4. `CLOSE_LUKS_MAPPER`
5. `BALANCE_TO_RAID1`
6. `VERIFY_POOL_HEALTH`
7. `VERIFY_EXPECTED_DISK_SET`

**Optional explicit action for operator-approved missing removal:**

1. `REMOVE_DISK_MISSING_EXPLICIT`

## 5. Declarative Model Rules

1. Keep config schema end-state oriented (`disks`, mountpoint, pool settings).
2. Do not introduce persistent intent states like `new/existing/replace/remove` in config.
3. Do not require users to mutate config just to advance a lifecycle phase that has already completed.

## 6. Checkpoint and Plan Semantics

1. Keep inline planning for fresh apply (no mandatory external plan artifact).
2. Keep archived history (`/var/lib/braid/history/`) for auditability.
3. On resume:
   - config hash mismatch must refuse
   - action preconditions re-checked at runtime
4. Apply state file remains authoritative only for in-progress execution, not long-term disk identity.

## 7. Standalone Script Compatibility

### 7.1 `braid-add-disk.sh`

Current behavior conflicts with final architecture because it performs formatting implicitly.

**Policy:**

1. Mark as deprecated immediately.
2. Preferred path:
   - `braid init-disk <by-id>`
   - update config / rebuild
   - `braid apply`
3. Compatibility period option:
   - keep `braid-add-disk.sh` as thin wrapper that calls `braid init-disk` then `braid apply`
   - no private formatting logic remains in standalone script

### 7.2 Other standalone scripts

1. `braid-remove-disk` and `braid-status` may remain temporarily.
2. They must not reintroduce formatting paths.

## 8. Documentation Updates Required

This proposal changes behavior and invariants; update docs in same PR:

1. `docs/principles.md`
   - clarify destructive boundary: init-only formatting, apply-safe reconciliation.
2. `docs/decisions/config-first-workflow.md`
   - replace old add-disk flow with init-disk + apply.
3. `docs/decisions/unified-cli.md`
   - add `init-disk` command and action model changes.
4. `README.md`
   - cookbook steps for first disk, add disk, degraded/missing disk behavior.
5. Add new decision doc if needed:
   - `docs/decisions/safe-by-construction-reconciliation.md` with explicit status.

## 9. Prescriptive Error and Warning Behavior

### 9.1 Required warning classes

1. `DISK_ABSENT_SKIPPED`: configured by-id path not present; disk skipped for this run.
2. `POOL_DEGRADED_MISSING_DEVICES`: pool has missing devices.

### 9.2 Required blocking classes

1. `INIT_REQUIRED`: apply blocked because add path requires explicit init-disk first.
2. `RESUME_TARGET_MISSING`: resume cannot continue because pending action target absent.
3. `CHECKPOINT_CONFIG_DRIFT`: existing hash mismatch behavior.

### 9.3 Message quality requirements

Every warning/error must include:

1. exact disk by-id path
2. operation being skipped/refused
3. one-line remediation command

## 10. Implementation Plan (File-Level)

### 10.1 `scripts/braid.sh`

1. Add `init-disk` subcommand parser and handler.
2. Implement `action_open_luks` (non-destructive).
3. Remove `action_luks_format_open` from apply path.
4. Update planner logic:
   - stop generating format actions
   - detect absent/non-LUKS conditions explicitly
   - classify as warnings or blocked reasons
5. Update apply executor:
   - handle `OPEN_LUKS`
   - enforce runtime preconditions per action
   - enforce resume target-missing failure behavior
6. Gate missing-device removal behind explicit intent path.

### 10.2 `modules/braid/cli.nix`

1. Ensure runtime dependencies for `init-disk` flow are present (`cryptsetup`, `jq`, `btrfs-progs`, etc.).
2. Verify command exposure includes new subcommand.

### 10.3 Compatibility wrappers/scripts

1. Rewire or deprecate `braid-add-disk.sh` to avoid direct formatting logic.
2. Emit deprecation warning with migration command sequence.

### 10.4 Docs

Update files in section 8 in same implementation cycle.

## 11. Complete Test Plan (NixOS VM)

All tests below are required and must include assertions on both behavior and safety invariants.

### 11.1 New tests for `init-disk`

1. `init-disk formats declared non-LUKS disk`.
2. `init-disk refuses undeclared disk`.
3. `init-disk refuses already-LUKS disk without --force`.
4. `init-disk --force requires confirmation phrase`.
5. `init-disk refuses formatting disk currently in pool`.
6. `init-disk enforces single-passphrase check against existing pool member`.

### 11.2 Planner tests (read-only behavior)

1. Config disk present + non-LUKS => plan marked blocked with `INIT_REQUIRED`, no mutation action for that disk.
2. Config disk absent => `DISK_ABSENT_SKIPPED` warning, plan remains applicable if other actions valid.
3. No path emits `ADD_DISK_LUKS_FORMAT_OPEN`.
4. Missing pool members do not auto-emit inferred destructive remove actions.
5. Human output includes warnings and remediation text.
6. JSON schema contains `status` (`applicable/blocked`) and `blocked_reasons[]`.

### 11.3 Apply tests (fresh run)

1. `apply` with absent configured disk continues other non-destructive work and warns.
2. `apply` with present non-LUKS disk refuses with `INIT_REQUIRED` and does not mutate disk.
3. `apply` after successful `init-disk` opens and adds disk correctly.
4. `apply` never calls `luksFormat` (assert by log inspection or command stubbing).
5. Degraded pool with unrelated add operation proceeds and preserves existing data.

### 11.4 Resume and checkpoint tests

1. Interrupted apply resumes successfully when config unchanged and targets present.
2. Resume fails on config hash drift (existing behavior).
3. Resume fails when pending action target disk becomes absent (`RESUME_TARGET_MISSING`).
4. Failed resume preserves checkpoint for retry.

### 11.5 Explicit missing-device removal tests

1. Missing-device removal refused without explicit operator intent.
2. Explicitly authorized removal succeeds and clears missing entry.
3. Ambiguous multi-missing scenario refuses with actionable error.
4. Removal path still honors redundancy confirmation when dropping below two devices.

### 11.6 Backward compatibility tests

1. Legacy `braid-add-disk.sh` path prints deprecation warning.
2. Wrapper mode (`init-disk` + `apply`) still results in successful add.
3. Legacy behavior cannot trigger implicit formatting from `apply`.

### 11.7 Regression tests for original failure class

1. Single-disk pool disk unplugged, run `apply`: no format, warning only.
2. Replug same disk, run `apply`: opens/reconciles without data loss.
3. Validate sentinel file hash unchanged across unplug/apply/replug/apply cycle.

## 12. Acceptance Criteria

Implementation is complete only when all are true:

1. `grep`/code inspection confirms no `luksFormat` reachable from `braid apply` path.
2. All tests in section 11 pass.
3. Docs updated per section 8.
4. Operator can execute full lifecycle with cookbook commands and no hidden state.

## 13. Canonical Operator Workflows

### 13.1 First disk bootstrap

1. Add disk by-id to Nix config.
2. `nixos-rebuild switch`.
3. `BRAID_PASSPHRASE=... braid init-disk /dev/disk/by-id/<disk1>`
4. `BRAID_PASSPHRASE=... braid apply`

### 13.2 Add second disk

1. Add disk by-id to config.
2. `nixos-rebuild switch`.
3. `BRAID_PASSPHRASE=... braid init-disk /dev/disk/by-id/<disk2>`
4. `BRAID_PASSPHRASE=... braid apply`

### 13.3 Disk temporarily absent

1. Run `braid apply`.
2. Observe skip+warning for absent disk; other safe operations continue.
3. Reconnect disk.
4. Run `BRAID_PASSPHRASE=... braid apply` to reconcile.

### 13.4 Replace failed disk

1. Remove failed disk from config (or mark explicit removal path).
2. Add new disk to config.
3. `nixos-rebuild switch`.
4. `BRAID_PASSPHRASE=... braid init-disk /dev/disk/by-id/<new-disk>`
5. `BRAID_PASSPHRASE=... BRAID_CONFIRM='remove missing device from pool' braid apply --allow-remove-missing`

---

This proposal is the authoritative target behavior for implementation.

## 14. Implementation Order (Mandatory)

Follow this sequence exactly. Do not reorder steps. Do not skip tests between phases.

### 14.1 Execution rules

1. Implement one phase at a time.
2. At the end of each phase, run only the listed tests for that phase first.
3. Do not start the next phase until all phase tests pass.
4. If a phase requires docs updates, include them in the same commit as behavior changes.
5. If any behavior conflicts with this document, this document wins.

### 14.2 Ordered phases

1. **Phase 1: Add `init-disk` command skeleton and parser wiring**
   - Files: `scripts/braid.sh`, `modules/braid/cli.nix` (if command exposure/runtime wiring needed)
   - Deliverable: command exists with usage/help and argument parsing (`--force`, `--config`)
   - Tests: new minimal command-dispatch test

2. **Phase 2: Implement `init-disk` safety contract**
   - Implement section 4.1 checks in order (declared-disk requirement, pool-membership refusal, isLuks refusal, force-confirmation, passphrase check, format)
   - Deliverable: destructive path isolated to `init-disk` only
   - Tests: section 11.1 (all)

3. **Phase 3: Remove format action from plan/apply model**
   - Delete planner emission and executor handling of `ADD_DISK_LUKS_FORMAT_OPEN`
   - Add `OPEN_LUKS` action and handler
   - Deliverable: no `luksFormat` reachable from `apply`
   - Tests: section 11.2 items 1-4, section 11.3 item 4

4. **Phase 4: Implement planner status model (`applicable` vs `blocked`)**
   - Add plan `status`, `blocked_reasons[]`, warning codes
   - Non-LUKS configured-present disk must produce blocked `INIT_REQUIRED`
   - Deliverable: plan JSON + human output reflect new status/warnings model
   - Tests: section 11.2 (all)

5. **Phase 5: Implement missing-disk policy (`skip+warn` fresh apply)**
   - Fresh apply skips absent configured disks and continues unrelated actions
   - No destructive fallback from absence
   - Deliverable: degraded-tolerant fresh reconciliation
   - Tests: section 11.3 items 1 and 5, section 11.7 (all)

6. **Phase 6: Implement explicit missing-device removal gate**
   - Add `--allow-remove-missing` to both `braid plan` and `braid apply`
   - Require `BRAID_CONFIRM='remove missing device from pool'` for missing-device eviction
   - Emit `REMOVE_DISK_MISSING_EXPLICIT` only when gate is active
   - Deliverable: explicit-only missing eviction behavior
   - Tests: section 11.5 (all), plus plan preview with/without gate

7. **Phase 7: Tighten resume semantics**
   - On resume, fail if pending action target is absent (`RESUME_TARGET_MISSING`)
   - Preserve checkpoint on failure
   - Deliverable: strict in-flight action integrity
   - Tests: section 11.4 (all)

8. **Phase 8: Compatibility path for `braid-add-disk.sh`**
   - Deprecate script and/or rewire as wrapper (`init-disk` + `apply`)
   - Remove standalone hidden formatting logic
   - Deliverable: no alternative implicit-format path
   - Tests: section 11.6 (all)

9. **Phase 9: Documentation synchronization**
   - Update files in section 8
   - Ensure README workflows match section 13 exactly
   - Deliverable: docs and behavior aligned
   - Tests: docs review + command examples sanity run

10. **Phase 10: Final verification gate**
    - Run full targeted suite from section 11
    - Run grep/code audit for `luksFormat` reachability from apply path
    - Confirm acceptance criteria section 12 all true
    - Deliverable: release-ready implementation
