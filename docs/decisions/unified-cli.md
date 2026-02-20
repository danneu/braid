# Decision: Unified CLI with Plan/Apply

Status: Active

> Principle: [Config-first workflow](../principles.md#2-config-first-workflow)

## Context

Braid has three standalone scripts (`braid-add-disk`, `braid-remove-disk`, `braid-status`). Each handles one operation with its own validation, pool probing, and confirmation flow. The config-first workflow (edit config → rebuild → run script) is sound, but operators must choose the right script and remember its flags.

A unified `braid` command with `plan` (dry-run diff) and `apply` (execute with checkpoints) replaces the multi-script mental model with one flow: `edit config → rebuild → plan → apply`.

## Options considered

1. **Keep separate scripts** — add `braid-plan` as a fourth script. Simple but doesn't unify the execute path or add checkpoint/resume.
2. **Go binary** — full rewrite in Go. Better for complex state machines, but high migration risk and slower delivery for equivalent behavior.
3. **Bash+jq unified script** — single `braid` dispatcher with subcommands. Reuses existing tested patterns. JSON plan/checkpoint formats work with jq.

## Decision

Option 3. Bash+jq for phases 1-3. Re-evaluate language choice only after plan/apply semantics are stable.

### Architecture

Single script `scripts/braid.sh` with subcommand dispatcher:

- `braid plan [--json] [--config <path>]` — read-only diff: desired state (config) vs live state (LUKS/btrfs/mounts). Outputs action list.
- `braid apply [--resume] [--config <path>]` — executes plan with checkpoint persistence. `--resume` continues from `/var/lib/braid/apply-state.json`.
- `braid status [--verbose] [--json] [--config <path>]` — pool health summary (replaces `braid-status`).

Packaged via `pkgs.writeShellApplication` in `cli.nix`, same as existing scripts.

### Plan/apply state machine

1. `braid plan` produces a JSON plan (action list with types, targets, preconditions)
2. `braid apply` runs the planner internally, writes checkpoint, executes actions in order
3. Each action updates the checkpoint atomically (write tmp + mv)
4. On success: checkpoint moves to `/var/lib/braid/history/<plan_id>.json`, active file removed
5. On failure: checkpoint stays for `--resume`
6. `--resume` verifies config hash matches before continuing

### Action types

- `ADD_DISK_LUKS_FORMAT_OPEN` — LUKS format + open
- `ADD_DISK_BTRFS_ADD` — add mapper to btrfs pool
- `BALANCE_TO_RAID1` — convert pool to RAID1 profile
- `REMOVE_DISK_GRACEFUL` — btrfs device remove (data migrates)
- `REMOVE_DISK_MISSING` — btrfs device remove missing
- `CLOSE_LUKS_MAPPER` — cryptsetup close
- `VERIFY_POOL_HEALTH` — confirm pool state matches expectations
- `VERIFY_EXPECTED_DISK_SET` — confirm pool members match config

### Checkpoint schema

Active: `/var/lib/braid/apply-state.json`
History: `/var/lib/braid/history/<plan_id>.json` (last 20 retained)

### Backward compatibility

Existing scripts remain installed and functional during transition. No wrapper changes needed — the old scripts work independently.

## Constraint

Two commands (`plan` then `apply`) instead of one. This is intentional — deterministic dry-run before mutation prevents accidents.

## See

- `plans/disk-migration-system.md` — full migration plan with edge cases
- `docs/decisions/config-first-workflow.md` — config-first principle this builds on
- `docs/decisions/disk-pool-management.md` — existing pool management spec
