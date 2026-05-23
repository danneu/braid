[← braid](../index.md)

# braid status

Show pool health, per-disk detail, capacity, and operation progress.

## When to use it

- After unlocking, to verify everything is healthy
- To check on a running scrub, balance, or replace
- To find device IDs needed by other commands (`--missing-id`)
- To investigate alerts or degraded state

## Basic example

```
sudo braid status
```

## Common variations

Machine-readable JSON output:

```
sudo braid status --json
```

## Important flags

| Flag | Purpose |
|---|---|
| `--json` | Output the full status report as JSON |

## Output sections

### Pool summary

```
Pool:     /mnt/storage
Status:   intact
```

Status values:

| Status | Meaning |
|---|---|
| **intact** | All disks present, no issues |
| **DEGRADED (N missing devices)** | One or more disks are missing; new writes have no redundancy for the missing device's data |
| **not mounted** | Pool is offline (LUKS closed or not mounted) |

### Alert banner

When a health alert is active, a banner appears at the top of the output:

```
ALERT -- disk health issue detected. Run 'braid ack' to acknowledge and silence.
  - btrfs device errors on toshiba1 (devid 1)
  - SMART health warning
```

Alert causes include btrfs device errors, missing devices, and SMART health warnings. Alerts are latched -- they persist until acknowledged with `braid ack`, even if the underlying condition resolves.

### Allocation table

Shows how data is distributed across block group types:

```
Allocation:
  Type       Profile  Used        Allocated
  Data       RAID1    1.20 TiB    1.50 TiB
  Metadata   RAID1    512.00 MiB  1.00 GiB
  System     RAID1    64.00 KiB   32.00 MiB
```

### Capacity

```
Capacity:
  Total:  10.91 TiB (Estimated)
  Used:   1.20 TiB
  Free:   9.50 TiB
```

For RAID1, the total is estimated as the effective mirrored capacity (not raw disk sum). With mismatched disk sizes, the oversized portion of the largest drive cannot be fully mirrored. The estimate accounts for this.

Total is omitted when the pool is degraded (the estimate would be misleading with missing devices).

### Drives (compact listing)

```
Drives:
  toshiba1     sda  devid=1  present
  toshiba2     sdb  devid=2  present
  toshiba3     -    -        missing
```

### Balance progress

Shown only when a balance is running or paused:

```
Balance:  running, 3/10 chunks (30% complete)
Balance:  paused, 5/12 chunks (58% complete)
```

### Last scrub result

```
Last scrub: Mon Jan  1 00:00:00 2024 (no errors)
Last scrub: Mon Jan  1 00:00:00 2024 cancelled (will resume)
Last scrub: Mon Jan  1 00:00:00 2024 interrupted
Last scrub: never
Last scrub: running (45%)
```

### Per-disk detail

Each disk shows its device path, model, serial, LUKS UUID, and I/O error counts:

```
Disks:

  toshiba1          devid 1   present
    Device:  /dev/disk/by-id/ata-TOSHIBA_MN07ACA12T_1234
    Model:   TOSHIBA MN07ACA12T
    Serial:  1234ABC
    LUKS:    aaaaaaaa-1111-2222-3333-444444444444
    Errors:  read=0 write=0 flush=0 corruption=0 generation=0

  toshiba3          MISSING
    Device:  /dev/disk/by-id/ata-TOSHIBA_MN07ACA12T_5678  (not found)
```

Disk states in the detail view:

| State | Meaning |
|---|---|
| **present** | Disk is online and healthy |
| **MISSING** | Disk not found at its by-id path |
| **LUKS HEADER UNREADABLE** | Device present but LUKS header cannot be read |
| **LUKS HEADER DAMAGED** | Device present but LUKS header is damaged |
| **UNKNOWN** | State could not be determined |

### Advisories

`braid status` may print one or more `warning:` lines above the pool
summary. Each warning corresponds to an entry in the JSON `advisories`
array.

**Foreign filesystem at the mount point.** When something other than
the braid pool is mounted at the configured mount point (for example, a
stale `tmpfs` or `ext4` mount left by another tool), `braid status`
reports `Status: not mounted` and names the actual filesystem type:

```
warning: /mnt/storage is mounted but fstype is ext4, not btrfs
```

Unmount the foreign filesystem before retrying `braid unlock` --
otherwise `unlock` reports "pool already mounted" because something is
in fact mounted at that path.

**Pending recovery journal.** When `/var/lib/braid/pending-op.json`
exists, an interrupted `add` / `remove` / `remove-missing` / `replace`
is owed. `braid status` prints the advisory whether or not the pool is
mounted:

```
warning: interrupted operation detected (pending-op.json exists, started 2026-05-20T10:30:00Z) -- run 'braid recover' to reconcile
```

Run `sudo braid recover` to reconcile from live pool state; do not
remove `pending-op.json` by hand except under the conditions documented
in [Pending-op file corruption](../guides/recovery-scenarios.md#pending-op-file-corruption).
If the journal is unreadable, the advisory carries the canonical
manual-reconciliation phrase instead -- because `braid recover` cannot
load an unparseable journal either:

```
warning: failed to parse pending-op.json: <detail>. Remove /var/lib/braid/pending-op.json after manual reconciliation (see docs/internals/luks-unlock.md) and re-run.
```

See [Unparseable state-file reconciliation](../internals/luks-unlock.md#unparseable-state-file-reconciliation)
for the safe-to-remove conditions.

**Pending LUKS header backups.** When a header-mutating operation
(`braid add`, `braid replace`, `braid enroll`) writes a local LUKS
header backup to `/var/lib/braid/luks-headers/<disk>.luksheader`,
`braid status` prints a warning until those files are removed:

```
warning: LUKS header backups exist in /var/lib/braid/luks-headers -- copy offsite and delete local copies
```

The local copy is a transient byproduct of the header-mutating
operation, not the intended backup target. Copy each `.luksheader`
file to an off-system location (USB, another machine, cloud key
storage), then remove the local copy to silence the warning.

See [LUKS header backup workflow](../internals/luks-unlock.md#header-backup-workflow-and-messaging)
for the full rationale.

## JSON output

`--json` produces a structured report suitable for monitoring tools. Key fields:

- `status`: `"intact"`, `"degraded"`, or `"not_mounted"`
- `disks`: array of disk reports with `name`, `status`, `devid`, `errors`, etc.
- `alert_active`: boolean
- `alert_causes`: array of alert cause objects
- `advisories`: array of human-readable advisory strings (omitted when
  none). See the Advisories section above for what currently produces
  them.
- `missing_devids`: array of every devid counted in `missing_count`
  (btrfs-MISSING devices and null-underlying mappers whose backing device has
  disappeared). For destructive `remove-missing` / `replace --missing-id`
  workflows, see those commands' notes -- a null-underlying devid here will be
  rejected by those commands until btrfs promotes it to MISSING.
- `capacity`: `total_bytes`, `used_bytes`, `free_bytes`
- `allocation`: array of block group type entries
- `balance`: state object (`idle`, `running`, `paused`, `unknown`)
- `last_scrub`: state object (`never`, `running`, `finished`, `aborted`,
  `interrupted`, `unknown`)

## Related commands

- [braid unlock](unlock.md) -- bring the pool online
- [braid replace](replace.md) -- repair a degraded pool
- [braid remove-missing](remove-missing.md) -- forget a dead device
  (operates only on btrfs-authoritative MISSING devids; see that command's note
  on transient null-underlying state)
- [braid idle](idle.md) -- machine-friendly idle/busy check for autosuspend
