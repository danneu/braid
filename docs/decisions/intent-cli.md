# Intent CLI

**Status: Active** — Supersedes `unified-cli.md` and `two-phase-apply.md`.

## Context

Braid's plan/apply reconciliation engine was over-engineered for NAS drives, which have ~4 events in their lifetime (create pool, add disk, add another, replace a dead one). The generic reconciler created problems:

- **Risk flattening**: routine reboot and adding a disk produced the same output format (a "plan" with "actions")
- **Combinatorial complexity**: `--allow-remove-missing`, `--allow-remove-ambiguous`, `BRAID_CONFIRM='phrase1;phrase2'`
- **Ceremony for routine operations**: `braid apply` after every reboot

## Decision

Replace plan/apply with four intent commands:

| Command | Purpose | Risk |
|---------|---------|------|
| `braid add <name>` | Format + join pool, or join existing LUKS device | Destructive (new disk) or safe (existing LUKS) |
| `braid remove <name>` | Migrate data off, detach from pool | Long-running |
| `braid replace --old <name> --new <name>` | Add new, rebalance, then evict dead | Transactional (add-first ordering) |
| `braid status` | Display pool health and disk info | Read-only |

### Named disks

Config changed from a list of by-id paths to a named attrset:

```nix
braid.disks = {
  toshiba  = { byId = "/dev/disk/by-id/ata-Toshiba_MN07_XXXX"; };
  ironwolf = { byId = "/dev/disk/by-id/ata-Ironwolf_ST12_YYYY"; };
};
```

Mapper names are `braid-<name>` (e.g., `braid-toshiba`) — human-friendly, debuggable in lsblk/systemd logs, deterministic.

### Safety model

The old architecture used a structural code boundary — `luksFormat` was literally unreachable from `apply`. The new architecture replaces this with:

1. **Explicit operator intent**: user names a specific disk and confirms
2. **Superblock guard**: before any `mkfs.btrfs`, the code opens the LUKS device and checks for an existing btrfs superblock. If found, the device is a returning member and `add` becomes a no-op.
3. **Confirmation calibrated to risk**: destructive operations (LUKS format) require explicit confirmation; safe operations (opening existing LUKS, adding to pool) proceed after simple yes/no.

The btrfs superblock check is the "idempotent format primitive" described in `safe-by-construction-reconciliation.md` as a potential revisit trigger.

### Resumability

Per-command checkpoint (`/var/lib/braid/op-state.json`) with staleness rules:
- Config hash changed → invalidate
- Pool topology changed → invalidate
- Different command or args → invalidate
- btrfs balance and device remove are inherently resumable (btrfs tracks internal progress)

### NixOS-native automation

- systemd `braid-unlock.service` + `braid-pool.target` for post-boot unlock
- Activation script prints UUID-based advisory guidance on `nixos-rebuild switch`

## Rejected alternatives

- **Keep plan/apply with simpler flags**: Still risk-flattening. The core problem is that a generic reconciler treats "reboot recovery" and "add a new disk" as the same kind of operation.
- **Separate init-disk + apply**: The original approach. Created an artificial code boundary that was hard to explain and required ceremony for the common case.

## Consequences

- No backwards compatibility with v1 — project is unreleased
- Four commands instead of five (no init-disk, no plan, no apply)
- Every command supports `--dry-run` and `--yes` for scripting
- Tab completion returns disk names from config
