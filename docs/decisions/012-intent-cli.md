# Intent CLI

**Status: Active** — Supersedes `008-unified-cli.md` and `011-two-phase-apply.md`.

## Context

Braid's plan/apply reconciliation engine was over-engineered for NAS drives, which have ~4 events in their lifetime (create pool, add disk, add another, replace a dead one). The generic reconciler created problems:

- **Risk flattening**: routine reboot and adding a disk produced the same output format (a "plan" with "actions")
- **Combinatorial complexity**: `--allow-remove-missing`, `--allow-remove-ambiguous`, `BRAID_CONFIRM='phrase1;phrase2'`
- **Ceremony for routine operations**: `braid apply` after every reboot

## Decision

Replace plan/apply with five intent commands:

| Command | Purpose | Risk |
|---------|---------|------|
| `braid add <name=by_id>...` | Format + join pool, or recover identity-verified LUKS device | Destructive (new disk), safe (returning braid disk with matching FSID), or refused (non-braid LUKS, foreign pool, no pool to verify) |
| `braid remove <name>` | Migrate data off present disk, detach from pool | Long-running |
| `braid remove-missing --missing-id <devid>` | Clean up a stale missing-device entry; restores RAID1 profiles if this clears the last missing device | Long-running |
| `braid replace --old <name> --new <name=by_id>` | Replace a disk (live or dead) using `btrfs replace start`; restores RAID1 profiles for missing-path when clearing the last missing device | In-place swap (preserves devid) |
| `braid status` | Display pool health and disk info | Read-only |

### Disk keys

Disk membership is CLI-owned runtime state in `/var/lib/braid/pool.json` (see [017-runtime-disk-membership.md](017-runtime-disk-membership.md)). Disks are added with `name=by_id` syntax:

```sh
braid add toshiba=/dev/disk/by-id/ata-Toshiba_MN07_XXXX \
          ironwolf=/dev/disk/by-id/ata-Ironwolf_ST12_YYYY
```

Mapper names are `braid-<name>` (e.g., `braid-toshiba`) — human-friendly, debuggable in lsblk/systemd logs, deterministic.

### Safety model

The old architecture used a structural code boundary — `luksFormat` was literally unreachable from `apply`. The new architecture replaces this with:

1. **Explicit operator intent**: user specifies a disk key and confirms
2. **Layered identity check** for existing LUKS devices:
   a. LUKS label must be `braid-<key>` — non-braid LUKS is refused outright.
   b. Pool must be mounted — bootstrap refuses existing LUKS (no pool to verify against).
   c. Opened mapper's btrfs FSID must match the current pool — foreign-pool disks are refused.
   d. Braid-labeled LUKS with no btrfs superblock is refused -- this state is ambiguous (clean eviction, partial init, manual wipe, stale data) and cannot be distinguished without tombstones.
   e. A braid-labeled LUKS disk with a btrfs superblock whose FSID matches the mounted pool may be accepted as a returned-disk add target. The add journal records that identity before mutation. If the stale btrfs signature would block `btrfs device add`, braid runs only `wipefs --all --types btrfs` on the verified mapper and uses `btrfs device add -f`.
   f. Superblock guard remains as defense-in-depth within the FSID-matching path.
3. **Unified confirmation with device context**: all mutating commands (`add`, `remove`, `remove-missing`, `replace`) show a rich device-info block (model, size, serial via lsblk) and confirm with `Type 'yes' to continue:`. Degraded-path warnings are informational text, not special confirmation phrases. `--yes` skips the prompt for scripting.
4. **Disk name immutability**: mutating commands validate names against recorded disk identity and reject name rename/reassignment. Operators must use explicit `replace` or `remove`+`add` workflows instead of renaming.
5. **Journal-protected mutations**: mutating commands write `pending-op.json` before the first irreversible step; it is cleared only after the full operation (including follow-up work like soft balance) succeeds. Existing-pool add journals are phased: `PoolMutation` may finish target preparation and btrfs membership, while `PostAddBalanceRaid1` may only validate committed membership and finish the owed RAID1 balance. On any error exit, the journal persists to enable `braid recover`.

`--dry-run` performs side-effect-free, passphrase-free LUKS probes only -- LUKS label reads, and the keyfile credential test used by `braid enroll` (`cryptsetup open --test-passphrase --key-file`, which evaluates a credential without activating the device). Checks that require a passphrase or an open mapper -- e.g. full identity verification (FSID comparison) -- are deferred to execution time when the mapper is closed.

The dry-run preview itself stays on stdout. Side-effect-free probes that nevertheless do bound long-running work -- specifically the Argon2-bounded `--test-passphrase` evaluation in `braid enroll --dry-run` -- emit canonical `[wait]`/`[ok]`/`[skip]` status rows to stderr per [Principle 13. Announce long-running work](../principles.md#13-announce-long-running-work). The previous "successful dry-run leaves stderr empty" contract is intentionally relaxed for this case: an Argon2 derivation runs whether or not the user can see it, and silent dry-runs that take seconds-to-minutes look like hangs. The structured preview output is unchanged.

### Replace safety constraints

- `--old` accepts both live (present in pool) and dead/missing disks.
- Both paths use `btrfs replace start` — the sole replacement primitive. Live disks replace in-place; missing disks are rebuilt from RAID redundancy by devid.
- `--missing-id` is only valid when `--old` is dead/missing. Rejected with live `--old`. Validated against actual missing devids via `probe_missing_devids()`.
- When exactly one device is missing, the devid is auto-resolved. Multiple missing devices require explicit `--missing-id`.
- Mixed state (live `--old` + pool has missing devices) is rejected — operator must repair the missing device first with `braid replace --missing-id <devid>`. `braid remove-missing` is only for intentional cleanup (forgetting stale entries without rebuilding data).
- No replacement path uses `btrfs device add` or `btrfs balance` — those are for `braid add` only.

### ENOSPC pre-flight check

`remove` and `remove-missing` validate that surviving devices have enough
unallocated space to absorb the target device's allocations before invoking
`btrfs device remove`. Without this, btrfs will either ENOSPC instantly or
crash the filesystem to read-only mid-relocation (reproduced in
`tests/repro/`).

Single-survivor cases use a path-specific check:

- **`remove` (2→1):** the RAID1-aware relocation check does not apply
  (there is only one remaining device, not two). Instead, a single-
  survivor capacity check derives demand from `btrfs filesystem df`
  logical usage -- `data + 2 * metadata + 2 * system`, reflecting the
  post-balance single + DUP profile on one device -- and compares it
  to the survivor's `device_size - device_slack`.
- **`remove-missing` (1 present + 1 missing):** the check is skipped.
  In 2-device RAID1, the surviving device already mirrors all data,
  so no relocation is needed.

### NixOS-native automation

- systemd `braid-unlock.service` + `braid-pool.target` for post-boot unlock
- `braid-online.service` lifecycle owner (`ExecStop=braid lock`, `RemainAfterExit=yes`)

## Rejected alternatives

- **Keep plan/apply with simpler flags**: Still risk-flattening. The core problem is that a generic reconciler treats "reboot recovery" and "add a new disk" as the same kind of operation.
- **Separate init-disk + apply**: The original approach. Created an artificial code boundary that was hard to explain and required ceremony for the common case.

## Consequences

- Five commands instead of three (no init-disk, no plan, no apply; `remove` split into `remove` + `remove-missing`)
- Every command supports `--dry-run` and `--yes` for scripting
- Tab completion returns disk names from `pool.json`
