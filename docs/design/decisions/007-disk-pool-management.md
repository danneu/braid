---
intent: "Record the Disk Pool Management decision and rationale. Read before changing related behavior or docs."
status: Superseded
---
> Superseded by [012-intent-cli.md](012-intent-cli.md) and [017-runtime-disk-membership.md](017-runtime-disk-membership.md).

# Decision: Disk Pool Management


> Principle: [CLI-owned membership](../principles.md#2-cli-owned-membership)

## Context

`braid-add-disk` exists and is tested. The pool still needs graceful disk removal, status reporting, and a clear replace workflow. All operations must follow the same config-first pattern: edit `braid.disks` → `nixos-rebuild switch` → run CLI tool.

## Principle: config-first applies symmetrically

Config-first is not just for adding disks. Every pool mutation follows the same workflow:

- **Add:** declare disk in `braid.disks` → rebuild → `braid-add-disk`
- **Remove:** remove disk from `braid.disks` → rebuild → `braid-remove-disk`
- **Replace:** remove dead disk + add replacement in `braid.disks` → rebuild → `braid-add-disk`

Symmetric guards enforce this:

- `braid-add-disk` refuses disks **not** in config
- `braid-remove-disk` refuses disks **still** in config

## `braid-remove-disk` spec

### Three-tier logic

1. **Target mapper exists and is open**, verified to map to the requested by-id disk → graceful `btrfs device remove /dev/mapper/xxx` (migrates data off the device)
2. **Target is absent/unopenable** and pool shows a missing device → `btrfs device remove missing`
3. **Otherwise** → fail with clear diagnostic

Graceful remove is preferred when possible. It avoids relying on RAID1 reconstruction and eliminates ambiguity if more than one device is missing.

### LUKS cleanup

After btrfs remove, `cryptsetup close` the mapper. Best-effort:

- Success → print "disk fully released" (safe to physically pull)
- Failure (busy) → print actionable next steps (`lsof`/`fuser` + retry), exit non-zero

### No passphrase required

Remove does not need a passphrase. The disk is already unlocked or already gone. Root access + config guard + typed confirmation is sufficient.

### Confirmation

Normal remove (pool stays RAID1 with 2+ disks):

```
Type 'remove this disk' to confirm:
```

Removing would drop below 2 disks (losing redundancy):

```
WARNING: This leaves 1 disk with no RAID1 redundancy.
A single disk failure will cause data loss.

Type 'remove this disk without redundancy' to confirm:
```

Warn but allow dropping to 1 disk — consistent with the single-disk start story.

### Reboot-in-between safety

If the user reboots between `nixos-rebuild switch` (which removes the LUKS entry) and running `braid-remove-disk`, the disk won't auto-unlock. This is safe: principle #1 (resilient boot) ensures the system boots and is reachable via SSH. The pool requires explicit `--allow-degraded` (or `autoUnlock.allowDegraded`) to mount degraded. The CLI handles both paths (tier 1 if disk is still somehow open, tier 2 if it's absent).

## `braid-status` spec

### Default output

Pool health summary: drive count, RAID profile, total/used/free capacity, degraded/missing state, last scrub result. Per-disk detail: model, serial, mapper name, btrfs devid, read/write/corruption error counters, LUKS UUID, present/missing state.

### `--json`

Machine-readable output for monitoring and automation.

## Replace workflow

Replace uses `braid-add-disk`, which already auto-evicts missing devices during rebalance.

Workflow:

1. Remove dead disk from `braid.disks`, add replacement
2. `nixos-rebuild switch`
3. `sudo braid-add-disk /dev/disk/by-id/<new-disk>`

Auto-evict is specifically for missing/dead devices. Planned removal of a healthy disk uses `braid-remove-disk`.

## Future vision

Document only — do not build yet.

**Unified CLI:** `braid plan` (dry-run diff of config vs live state), `braid apply` (execute with checkpoints and resumability), `braid status`, `braid replace-disk <old> <new>`.

**Phased roadmap:**

1. Ship `braid-remove-disk` and `braid-status` (solid primitives)
2. Read-only planner (`braid plan`)
3. Executor with checkpoints (`braid apply`)
4. First-class `braid replace-disk`
5. `braid-status --json`

Nix config remains source of truth throughout. The workflow evolves from `edit → rebuild → script` to `edit → rebuild → plan → apply`, but the principle is unchanged.

## CLI shape

Separate scripts (`braid-add-disk`, `braid-remove-disk`, `braid-status`) — not a unified CLI yet. The unified `braid` command is future work that depends on proven primitives.

## See

- `modules/braid/options.nix` — `braid.disks` option definition
- `modules/braid/storage.nix` — config export and LUKS entry generation
- `docs/design/decisions/002-config-first-workflow.md` — original config-first decision
