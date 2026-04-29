[← Manual](../index.md)

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
| **NEW** | Disk detected but not yet part of the pool |

### Advisories

Warnings appear when LUKS header backups are missing for one or more disks.

## JSON output

`--json` produces a structured report suitable for monitoring tools. Key fields:

- `status`: `"intact"`, `"degraded"`, or `"not_mounted"`
- `disks`: array of disk reports with `name`, `status`, `devid`, `errors`, etc.
- `alert_active`: boolean
- `alert_causes`: array of alert cause objects
- `missing_devids`: array of every devid counted in `missing_count`
  (btrfs-MISSING devices and null-underlying mappers whose backing device has
  disappeared). For destructive `remove-missing` / `replace --missing-id`
  workflows, see those commands' notes -- a null-underlying devid here will be
  rejected by those commands until btrfs promotes it to MISSING.
- `capacity`: `total_bytes`, `used_bytes`, `free_bytes`
- `allocation`: array of block group type entries
- `balance`: state object (`idle`, `running`, `paused`, `unknown`)
- `last_scrub`: state object (`never`, `running`, `completed`, `unknown`)

## Related commands

- [braid unlock](unlock.md) -- bring the pool online
- [braid replace](replace.md) -- repair a degraded pool
- [braid remove-missing](remove-missing.md) -- forget a dead device
  (operates only on btrfs-authoritative MISSING devids; see that command's note
  on transient null-underlying state)
- [braid idle](idle.md) -- machine-friendly idle/busy check for autosuspend
