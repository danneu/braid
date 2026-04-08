# Decision: Unified CLI with Plan/Apply

Status: Superseded by [012-intent-cli.md](012-intent-cli.md)

> Principle: [CLI-owned membership](../principles.md#2-cli-owned-membership) (successor)

## Context

Braid had three standalone scripts (`braid-add-disk`, `braid-remove-disk`, `braid-status`). Each handled one operation with its own validation, pool probing, and confirmation flow. The config-first workflow (edit config → rebuild → run script) was sound, but operators had to choose the right script and remember its flags. All three are now replaced by the unified Rust CLI.

A unified `braid` command with `plan` (dry-run diff) and `apply` (execute with checkpoints) replaces the multi-script mental model with one flow: `edit config → rebuild → plan → apply`.

## Options considered

1. **Keep separate scripts** — add `braid-plan` as a fourth script. Simple but doesn't unify the execute path or add checkpoint/resume.
2. **Go binary** — full rewrite in Go. Better for complex state machines, but high migration risk and slower delivery for equivalent behavior.
3. **Bash+jq unified script** — single `braid` dispatcher with subcommands. Reuses existing tested patterns. JSON plan/checkpoint formats work with jq.

## Decision

Option 3. Initial implementation was bash+jq. Now replaced by Rust CLI (`cli/`).

### Architecture

Rust CLI (`cli/src/`) with subcommand dispatcher:

- `braid init-disk <by-id> [--force] [--config <path>]` — destructive one-shot: LUKS format a declared disk. Requires explicit operator intent. Never called from `apply`.
- `braid plan [--json] [--allow-remove-missing] [--allow-remove-ambiguous] [--config <path>]` — read-only diff: desired state (config) vs live state (LUKS/btrfs/mounts). Outputs action list with status (`applicable`/`blocked`), warnings, and blocked reasons.
- `braid apply [--resume] [--allow-remove-missing] [--allow-remove-ambiguous] [--config <path>]` — executes plan with checkpoint persistence. `--resume` continues from `/var/lib/braid/apply-state.json`. Never performs `luksFormat`.
- `braid status [--json] [--config <path>]` — pool health summary with per-disk detail (replaces `braid-status`).
- `braid doctor [--json] [--config <path>]` — run diagnostic checks against config and pool state (config file, schema, permissions, declared disks, data/metadata profile consistency). Reports ok/warn/fail per check.

Packaged via Crane + `makeWrapper` in `flake.nix`.

### Hard boundary

`cryptsetup luksFormat` is forbidden in the `plan` and `apply` code paths. Only `init-disk` may invoke `luksFormat`. See [009-safe-by-construction-reconciliation.md](009-safe-by-construction-reconciliation.md).

### Plan status model

Plan JSON includes:
- `status`: `applicable` (can be executed) or `blocked` (requires operator action first)
- `blocked_reasons[]`: list of reasons the plan cannot proceed (e.g., `INIT_REQUIRED`, `IDENTITY_AMBIGUOUS_ABSENT_DISK`)
- `warnings[]`: non-blocking issues (e.g., `DISK_ABSENT_SKIPPED`, `INIT_REQUIRED`, `POOL_DEGRADED`)
- `confirmations[]`: actions requiring explicit operator confirmation (e.g., redundancy loss). When multiple confirmations are required, provide all phrases semicolon-separated in `BRAID_CONFIRM` (e.g., `BRAID_CONFIRM='phrase one;phrase two'`). Whitespace around semicolons is trimmed.

### Plan/apply state machine

1. `braid plan` produces a JSON plan (action list with types, targets, preconditions)
2. `braid apply` runs the planner internally, writes checkpoint, executes actions in order
3. Each action updates the checkpoint atomically (write tmp + mv)
4. On success: checkpoint moves to `/var/lib/braid/history/<plan_id>.json`, active file removed
5. On failure: checkpoint stays for `--resume`
6. `--resume` verifies config hash matches before continuing
7. On resume, absent action targets fail with `RESUME_TARGET_MISSING` (strict in-flight integrity)

### Action types

- `OPEN_LUKS` — open existing LUKS device (non-destructive)
- `ADD_DISK_BTRFS_ADD` — add mapper to btrfs pool
- `BALANCE_TO_RAID1` — convert pool to RAID1 profile
- `REMOVE_DISK_GRACEFUL` — btrfs device remove (data migrates)
- `REMOVE_DISK_MISSING_EXPLICIT` — btrfs device remove missing (requires `--allow-remove-missing` + `BRAID_CONFIRM`)
- `CLOSE_LUKS_MAPPER` — cryptsetup close
- `VERIFY_POOL_HEALTH` — confirm pool state matches expectations
- `VERIFY_EXPECTED_DISK_SET` — confirm pool members match config

### Checkpoint schema

Active: `/var/lib/braid/apply-state.json`
History: `/var/lib/braid/history/<plan_id>.json` (last 20 retained)

### Backward compatibility

`braid-add-disk` is now an error stub that directs operators to `braid init-disk` + `braid apply`. `braid-status` is deleted (replaced by `braid status`). `braid-remove-disk` remains as a standalone legacy script (not yet ported to the Rust CLI).

## Constraint

Two commands (`plan` then `apply`) instead of one. This is intentional — deterministic dry-run before mutation prevents accidents.

## See

- `docs/decisions/002-config-first-workflow.md` — config-first principle this builds on
- `docs/decisions/009-safe-by-construction-reconciliation.md` — destructive boundary principle
- `docs/decisions/007-disk-pool-management.md` — existing pool management spec
- `cli/src/` — Rust CLI implementation
- `scripts/braid-add-disk.sh` — error stub directing to `init-disk` + `apply`
