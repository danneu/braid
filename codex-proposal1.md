# Codex Proposal 1: Intent-Driven `braid` Plan/Apply

## Goal

Define the safest and most correct long-term behavior for braid by removing inference-based destructive actions and replacing them with explicit operator intent.

This proposal is optimized for early development, where refactors are acceptable and correctness is the priority.

## Problem

Current behavior can infer `ADD_DISK_LUKS_FORMAT_OPEN` for a disk that is:

- Declared in config
- Temporarily absent from pool/device visibility
- Potentially still carrying valid data

This can produce accidental erase paths after unplug/replug or resume flows.

Root cause: planner ambiguity between:

- "brand-new disk to initialize"
- "known disk temporarily missing or failed"

## Design Principles

1. `braid plan` is always read-only.
2. `braid apply` must execute an explicit, immutable plan artifact.
3. Destructive actions require explicit config intent.
4. Runtime must enforce action preconditions (not just annotate them).
5. Unknown or ambiguous state must block destructive execution.

## Proposed Model

### 1) Explicit Disk Intent in Config

Replace implicit membership-only config with per-disk intent.

Example conceptual schema:

```json
{
  "mountPoint": "/mnt/storage",
  "pool": {
    "fsid": "optional-known-fsid"
  },
  "disks": [
    {
      "byId": "/dev/disk/by-id/ata-disk1",
      "intent": "existing"
    },
    {
      "byId": "/dev/disk/by-id/ata-disk2",
      "intent": "new"
    },
    {
      "byId": "/dev/disk/by-id/ata-disk3",
      "intent": "replace",
      "replaces": "/dev/disk/by-id/ata-disk-old"
    },
    {
      "byId": "/dev/disk/by-id/ata-disk4",
      "intent": "remove"
    }
  ]
}
```

Intent semantics:

- `existing`: must already belong to pool (or be recoverably known). Never auto-format.
- `new`: may be formatted and added.
- `replace`: explicit replacement workflow for a declared prior member.
- `remove`: explicit removal target.

### 2) Immutable Plan Artifact

`braid plan` outputs a plan file (JSON) with:

- `plan_id`
- `created_at`
- `config_hash`
- `live_state_hash`
- concrete ordered actions
- action preconditions and expected postconditions

`braid apply` should require that plan artifact (or default to most recent generated plan), and must refuse to silently regenerate a different plan.

### 3) Hard Safety Gate for Destructive Actions

Before any format/remove action:

- Validate device presence and stable identity.
- Validate action is allowed by config intent.
- Validate no ambiguity (missing/unknown conflict).
- Validate checkpoint continuation still matches plan/config/live constraints.

If any check fails: abort and keep checkpoint.

### 4) Durable Identity Registry

Maintain a braid metadata registry (for known disks) including:

- by-id path history
- LUKS UUID(s)
- btrfs fsid/device uuid mapping when known
- first-seen / last-seen timestamps
- retired/replaced markers

Purpose:

- Prevent previously known disks from being treated as "new".
- Enable safe replace workflows and richer diagnostics.

## Planner Rules (Key Behavior Changes)

1. If any `existing` disk is missing:
   - Produce blocked plan (non-applicable), not destructive actions.
   - Give explicit remediation: reconnect, mark replace, or mark remove.
2. `new` disk can produce format/add actions only if identity checks pass and disk is not already known as prior pool member.
3. `replace` requires explicit source-target mapping; planner emits deterministic replace sequence.
4. `remove` emits remove sequence only for explicitly targeted members.
5. `REMOVE_DISK_MISSING` must never be inferred unless it directly corresponds to explicit `remove`/`replace` intent.

## Apply State Machine

1. Load plan artifact.
2. Verify `config_hash` unchanged.
3. Recompute live fingerprint and compare against plan admissibility constraints.
4. For each action:
   - validate preconditions at runtime
   - mark in-progress
   - execute
   - validate postconditions
   - mark completed
5. On success: archive checkpoint + plan execution record.
6. On failure: keep checkpoint, emit exact failed invariant.

Resume:

- Must reuse same plan artifact.
- Must re-run preconditions for pending/in-progress actions.
- Must refuse if plan admissibility constraints are violated.

## UX Changes

- `braid plan` output distinguishes:
  - `applicable` plan
  - `blocked` plan (with required operator decisions)
- `braid apply` on blocked plan is refused.
- Destructive actions display explicit intent provenance:
  - "Allowed because disk intent is `new`"
  - "Allowed because disk intent is `replace` for X"

## Impact on Current Failure Scenario

Scenario: single-disk pool, disk accidentally unplugged, run `braid apply`.

With this design:

- Disk is `existing`.
- Planner detects `existing` missing.
- Plan is blocked; no format action emitted.
- Apply refuses to run destructive operations.
- Replugging disk resolves block without wipe risk.

## Migration Strategy

1. Introduce v2 config schema with intent fields.
2. Add strict compatibility layer:
   - legacy config maps to conservative defaults (`existing`) where safe.
   - if uncertain, block and require explicit migration.
3. Introduce plan artifact requirement in apply (feature flag first, then default).
4. Add identity registry and enforce anti-reformat rule.
5. Remove legacy inference path once tests pass.

## Test Plan Additions

Add/extend NixOS VM tests for:

1. Existing disk missing => blocked plan, no format action.
2. Resume after unplug/replug never formats `existing`.
3. `new` intent formats only truly new/unknown device.
4. Replace intent requires explicit mapping and performs safe sequence.
5. Remove missing only under explicit remove/replace intent.
6. Apply refuses if plan hash/admissibility constraints drift.
7. Destructive precondition failures leave checkpoint and do not partially violate invariants.

## Recommendation

Adopt this intent-driven architecture as the canonical braid workflow:

1. Declare intent in config.
2. Generate immutable plan.
3. Apply exact plan with strict runtime invariants.

This provides the strongest correctness guarantees and eliminates ambiguity-led disk erasure classes by design.
