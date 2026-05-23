---
intent: "Record the Config-First Workflow decision and rationale. Read before changing related behavior or docs."
status: Superseded
---
> Superseded by [017-runtime-disk-membership.md](017-runtime-disk-membership.md).

# Decision: Config-First Workflow


> Principle: [CLI-owned membership](../principles.md#2-cli-owned-membership) (successor)

## Context

NixOS is declarative — `nixos-rebuild switch` should describe the system's desired state. But `cryptsetup luksFormat` and `btrfs device add` are destructive one-shot operations that cannot be made idempotent. Re-running them would destroy data.

## Options considered

1. **Fully declarative** — module handles formatting in an activation script. Simple but catastrophic if re-run.
2. **Fully imperative** — script manages everything, module reads live state. Works but creates config drift (disk formatted but not in NixOS config; pool unlock breaks).
3. **Config-first hybrid** — declare disk in NixOS config (source of truth), rebuild (creates LUKS entries that fail gracefully), then run imperative script to format. Script refuses undeclared disks.

## Decision

Option 3. The NixOS config is the source of truth. The script is a one-shot executor that reads from it.

### Workflow

1. Add disk to `braid.disks`
2. `nixos-rebuild switch` — module exports `/etc/braid/config.json`, creates LUKS entries (which fail gracefully since disk isn't formatted yet)
3. `sudo braid init-disk /dev/disk/by-id/...` — reads config, verifies disk is declared, formats LUKS (explicit, one-shot)
4. `sudo braid apply` — opens LUKS, adds to btrfs pool, balances to RAID1 if applicable
5. Next reboot auto-unlocks

### Config export

The module writes `/etc/braid/config.json` via `environment.etc`. This is the single Nix→runtime bridge. All CLI tools read it by default. The file is built at `nixos-rebuild` time and is read-only at runtime.

### Config drift prevention

The script refuses to format disks not listed in `braid.disks`. Error message tells the user exactly what to add and which commands to run. This ensures every formatted disk has a corresponding LUKS entry for pool unlock.

## Symmetric guards

Config-first applies to all pool operations, not just add. The guard works in both directions:

- `braid init-disk` refuses disks **not** in `braid.disks`
- `braid apply` removes disks from pool when they are **no longer** in `braid.disks`

Remove workflow: remove disk from `braid.disks` → `nixos-rebuild switch` → `sudo braid apply`. See [007-disk-pool-management.md](007-disk-pool-management.md) for full spec.

## Constraint

Two-step process (rebuild + run script) instead of a single rebuild. This is the minimum viable approach given that LUKS formatting is destructive.

### Revisit trigger

If NixOS ever gets a `formatDevice` option type that can safely express one-shot destructive operations, this deviation can be revisited.

## See

- `modules/braid/options.nix` — `braid.disks` option definition
- `modules/braid/storage.nix` — config export and LUKS entry generation
- `cli/src/` — Rust CLI (`init-disk`, `plan`, `apply`, `status`)
- `scripts/braid-add-disk.sh` — error stub directing to `init-disk` + `apply`
- `archive/design-docs/1-nixos-best-practices.md` — original best practices analysis
