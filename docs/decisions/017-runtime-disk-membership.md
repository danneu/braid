# Decision: Runtime Disk Membership

Status: Active — Supersedes [002-config-first-workflow.md](002-config-first-workflow.md).

> Principle: [CLI-owned membership](../principles.md#2-cli-owned-membership)

## Context

The original design declared disk membership in `braid.disks` (NixOS config). Adding a drive required editing Nix config, running `nixos-rebuild switch`, then running `braid add <name>`. This was wrong: disk membership is operational state ("which drives are in my pool right now"), not system architecture ("what services should run on this machine"). Requiring a rebuild to add a drive added ceremony and created a category error — NixOS config is for declarative system shape, not mutable runtime state.

## Decision

Move disk membership to a CLI-owned runtime state file. The NixOS module provides infrastructure (mount point, services, toolchain). The CLI owns which disks are in the pool.

### State model

**`/var/lib/braid/pool.json`** — authoritative membership with enriched metadata:
```json
{
  "disks": {
    "toshiba": {
      "by_id": "/dev/disk/by-id/ata-TOSHIBA_...",
      "luks_uuid": "aaaa-bbbb-cccc-dddd",
      "devid": 1,
      "added_at": "2026-03-27T12:00:00Z"
    }
  }
}
```
The `luks_uuid`, `devid`, and `added_at` fields are populated after the btrfs membership change commits. `unlock` and `recover` keep `luks_uuid`/`devid` in sync with the live pool via `refresh_pool_metadata` / `enrich_from_pool_state`. `added_at` is historical state -- once set on a member, it is preserved across all subsequent writes (`unlock`, `recover`, `replace`, `add`, etc.). They replace the former `disk-map.json` advisory file.

**`/etc/braid/config.json`** — machine config (no disk information):
```json
{ "mount_point": "/mnt/storage" }
```

**`/var/lib/braid/pending-op.json`** — pending-operation journal (transient, present only during mutations).

### Mutation ordering

All mutating commands validate, write `pending-op.json` with pre/target membership snapshots, perform the irreversible btrfs membership change, write `pool.json` to reflect the committed live membership, then advance the journal to a post-maintenance phase before performing any required post-mutation maintenance and clearing the journal.

`pool.json` reflects committed btrfs membership, not necessarily completion of follow-up maintenance such as RAID1 rebalance or resize. While `pending-op.json` exists, `braid recover` is responsible for replaying or completing any owed post-mutation work before clearing the journal. Recovery in a post-maintenance phase must not rerun the primary btrfs membership command (`device add`, `device remove`, or `replace start`).

For `add`, membership commits when `btrfs device add` returns success; the post-add RAID1 balance is follow-up maintenance. For `remove`, membership commits when `btrfs device remove` returns success; writing `pool.json` before that would be wrong because btrfs still owns the device. For `remove-missing`, membership commits when `btrfs device remove <devid>` against the missing devid returns success; the post-remove soft balance that restores RAID1 redundancy for chunks created during degraded operation is follow-up maintenance. For `replace`, membership commits when `btrfs replace start -B` completes; the post-replace resize, and (for missing-path replacements that clear the last missing device) the soft balance, are follow-up maintenance.

The journal provides crash safety: if braid crashes mid-operation, the journal triggers recovery mode on next invocation. If a crash lands after `pool.json` was written but before the post-maintenance phase rewrite, `braid recover` detects the committed live topology, rewrites the journal to the post phase, and then finishes only the owed maintenance.

### Recovery mode

When `pending-op.json` exists, braid enters recovery mode. All commands except `status`, `recover`, and `lock` hard-fail. `braid recover` opens LUKS devices, mounts the pool (with `--allow-degraded` if needed), and rebuilds or repairs membership from the live btrfs pool topology -- not from LUKS label scanning, which could include labeled-but-never-added disks.

### State contract

- `pool.json` is authoritative. `unlock` requires it.
- `unlock` enriches `pool.json` metadata (luks_uuid, devid) on each mount via `refresh_pool_metadata`, but never changes membership (disk set).
- If `pool.json` is missing or corrupt, `unlock` fails with a clear error directing the user to `braid add` or `braid discover --write`.
- If `pool.json` is readable but stale (a member fails to probe), `unlock` warns and proceeds with the members it can probe. It never rewrites `pool.json`.
- If a member's stored `luks_uuid` doesn't match the probed device's LUKS UUID, `unlock` fatally errors. This catches swapped, reformatted, or corrupted drives before any LUKS open or mount is attempted.
- Only these commands write `pool.json` membership: `add`, `remove`, `replace`, `remove-missing`, `discover --write`, `recover`.

### Recovery

Recovery is always explicit, never implicit:
- `braid recover` opens LUKS devices and mounts the pool if needed. Mount membership is phase-specific: add/remove-missing pool-mutation phases mount from the pre-operation membership, add/remove-missing post phases mount from the committed target membership, replace pool-mutation recovery uses the pre/target union, and replace post-maintenance recovery mounts from the committed target membership. This is the only path out of recovery mode (journal present). It probes actual pool topology, not LUKS labels. Each live member's `by_id` is resolved at recovery time by walking `/dev/disk/by-id/` and matching the symlink whose canonical target equals the live device's backing kernel path -- `by_id` is never copied from the journal snapshot, which can be stale if hardware enumeration changed since the mutation started. If no by-id symlink resolves to a live pool member, recovery hard-fails with an actionable remediation message rather than persisting a guess. When rebuilding `pool.json`, recover preserves each member's `added_at` from the current `pool.json` if present, else from the journal's pre/target membership snapshot; only members with no prior timestamp get a fresh `now_iso()` stamp via `enrich_from_pool_state`. `by_id`, `luks_uuid`, and `devid` remain live-derived. When the pool is already mounted by an external process (circumventing `braid unlock`'s pending-op preflight) and the journal records `Replace::PoolMutation`, recovery refuses and directs the operator to `braid lock; braid recover` so a fresh mount session can be opened and the relock cycle can clear any kernel-resumed-`dev_replace` staleness. Replace post-maintenance recovery is allowed on an already-mounted pool because the primary replace has already committed.
- `braid discover` scans `/dev/disk/by-id/*` for LUKS devices with `braid-*` labels. Displays what it finds. With `--write`, persists to `pool.json`. This is for initial setup recovery (lost pool.json), not for crash recovery.
- The normal path to create `pool.json` is `braid add`.

### CLI syntax

`braid add` takes `name=by_id` positional pairs:
```
braid add toshiba=/dev/disk/by-id/ata-TOSHIBA wd=/dev/disk/by-id/ata-WDC
```

`braid replace --new` takes the same format:
```
braid replace --old toshiba --new seagate=/dev/disk/by-id/ata-Seagate_NEW
```

### Lifecycle model

The NixOS module no longer generates `fileSystems`, LUKS entries, or `btrfs-device-scan`. Instead:

- `braid-online.service` — lifecycle owner (`ExecStop=braid lock`, `RemainAfterExit=yes`). Started by the wrapper after a successful `unlock` or `add` that leaves the pool mounted.
- `braid-pool.target` — wants unlock only, does not start `braid-online` directly.
- Consumer services bind to `mnt-storage.mount` (auto-generated by systemd from `/proc/mounts`).

## Rejected alternatives

1. **Keep `braid.disks` but make it optional** — half-measure that leaves two sources of truth. Users would be confused about which one matters.
2. **Auto-discover on unlock** — makes `unlock` a mutation command. If discovery finds the wrong devices (e.g., a test disk with a `braid-*` label), the pool is corrupted silently. Explicit membership is safer.
3. **Store membership in btrfs metadata** — btrfs doesn't have a user-data field on devices. Would require a convention (e.g., subvolume with a JSON file), adding fragility and a chicken-and-egg problem for `unlock`.

## Consequences

- Adding a drive is one command: `braid add name=/dev/disk/by-id/...`. No `nixos-rebuild`.
- `pool.json` must exist before `unlock` can run. First-time setup: `braid add` creates it.
- `braid discover --write` is the explicit recovery path for lost/corrupt `pool.json`.
- The NixOS module's `braid.disks` option is removed entirely.

## See

- `cli/src/membership.rs` — load/save/validate membership, `DiskMember`, `refresh_pool_metadata`
- `cli/src/journal.rs` — pending-operation journal (pre/target membership snapshots)
- `cli/src/recover.rs` — rebuild membership from live pool state
- `cli/src/preflight.rs` — `check_no_pending_operation` recovery mode guard
- `cli/src/discover.rs` — LUKS label scanning
- `modules/braid/storage.nix` — `braid-online.service`, no `fileSystems`
- `modules/braid/options.nix` — no `braid.disks`
