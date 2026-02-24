# Decision: Safe-by-Construction Reconciliation

Status: Active

> Principle: [Safe-by-construction reconciliation](../principles.md#3-safe-by-construction-reconciliation)

## Context

`braid apply` originally mixed two fundamentally different operation classes:

1. **One-time destructive initialization** — `cryptsetup luksFormat` destroys all data on the target device. It is not idempotent. Running it twice destroys a working LUKS volume.
2. **Repeatable reconciliation** — `cryptsetup luksOpen`, `btrfs device add/remove`, balance, verify. These are safe to run repeatedly.

The structural hazard: state ambiguity (temporarily absent disk vs truly new disk) can route execution toward formatting. A disk that was unplugged and replugged could be misidentified as "new" and reformatted, destroying data.

## Options considered

1. **Registry-based** — track disk identity in a persistent registry to distinguish "new" from "returning". Adds hidden state, drift risk, and recovery complexity.
2. **Config flags** — add lifecycle state (`new`/`existing`/`replace`) to NixOS config. Violates declarative end-state principle — config becomes imperative.
3. **Structural separation** — move destructive operations to a separate command that requires explicit operator intent. `apply` physically cannot format.

## Decision

Option 3. Hard boundary between destructive initialization and safe reconciliation.

### Architecture

- `braid init-disk <by-id>` — the only command that may call `cryptsetup luksFormat`. Requires the disk to be declared in config, not already LUKS-formatted (unless `--force`), and not in an active pool. Enforces single-passphrase invariant.
- `braid plan` / `braid apply` — reconciliation only. Emits `OPEN_LUKS` (non-destructive open), never `luksFormat`. Non-LUKS disks produce an `INIT_REQUIRED` warning telling the operator to run `init-disk` first.

### Hard boundary enforcement

`cryptsetup luksFormat` is forbidden in the plan/apply code path. This is verified by:
1. Code inspection — no `luksFormat` call exists in `compute_plan()`, executor dispatch, or any function reachable from `cmd_apply()`.
2. Test assertion — `braid-apply.py` includes an explicit test that `apply` never contains `luksFormat`.
3. The `ADD_DISK_LUKS_FORMAT_OPEN` action type has been removed from the planner and executor entirely.

### Missing-disk policy

Absent configured disks are skipped with a `DISK_ABSENT_SKIPPED` warning. The plan remains `applicable` — other safe operations proceed. This prevents a temporarily disconnected disk from blocking all reconciliation.

Missing pool devices (devices in btrfs but not in config) require explicit operator intent to evict: `--allow-remove-missing` flag plus `BRAID_CONFIRM='remove missing device from pool'` environment variable. This prevents accidental eviction of temporarily absent disks.

Device identity is established by LUKS UUID, not by path or mapper name. When a config disk is absent, its UUID is unknowable, creating identity ambiguity for removal decisions. If the planner wants to remove a pool device but cannot verify it doesn't match an absent config disk, the plan is blocked with `IDENTITY_AMBIGUOUS_ABSENT_DISK`. The operator can override with `--allow-remove-ambiguous` plus `BRAID_CONFIRM='remove despite ambiguous identity'`.

### Resume strictness

Fresh `apply` is tolerant of absent disks (skip + warn). But checkpointed in-flight actions are strict: if a pending action's target becomes absent during `--resume`, the apply fails with `RESUME_TARGET_MISSING`. The checkpoint is preserved for retry after the target is restored.

## Constraint

Two commands (`init-disk` + `apply`) instead of one. Operators must explicitly initialize each new disk before reconciliation can include it. This is the minimum viable approach given that LUKS formatting is destructive and non-idempotent.

### Revisit trigger

If NixOS or LUKS ever provides an idempotent "ensure formatted" primitive that is safe to run on an already-formatted device, the separation can be revisited.

## See

- `cli/src/` — Rust CLI (`init-disk`, `plan`, `apply`, `status`)
- [config-first-workflow.md](config-first-workflow.md) — config-first principle
- [unified-cli.md](unified-cli.md) — plan/apply architecture and action types
