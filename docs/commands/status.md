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
FSID:     <uuid>
Profile:
  Data:      RAID1
  Metadata:  RAID1
  System:    RAID1
```

Status values:

| Status | Meaning |
|---|---|
| **intact** | All disks present, no issues |
| **DEGRADED (N missing devices)** | One or more disks are missing; redundancy is reduced on the missing device's data |
| **not mounted** | Pool is offline (LUKS closed or not mounted) |

Profile section:

`Profile:` summarizes btrfs profiles per block-group type. btrfs profiles are
per type, so Data, Metadata, and System can differ; see
[btrfs balance profiles](../internals/btrfs/balance-profiles.md) for the
background.

| Per-type rendering | Meaning |
|---|---|
| `RAID1` (also `RAID1C3`, `RAID1C4`, `RAID10`) | Mirrored across drives; reads self-heal from the redundant copy. |
| `DUP (same-disk copies; no disk redundancy)` | Two copies on the same physical device, the default metadata/system profile on a 1-device pool. Survives bit-rot, not device failure. |
| `single (no redundancy)` (also `RAID0 (no redundancy)`) | One copy across the affected block groups. Checksums detect bit-rot, but corruption cannot be repaired. |
| `single, RAID1 (not fully redundant)` | Block groups for this type span more than one profile, typically after an interrupted balance or degraded writes. Run `braid doctor` for the right next step; doctor recommends a soft RAID1 balance on a healthy pool and `braid replace` first on a degraded pool. |
| `unknown` | No block groups of this type were reported. Check `braid status` advisories for a df probe failure. |
| `RAID5`, `RAID6`, or any unrecognized name | braid does not classify parity profiles or future btrfs profiles. The raw profile name is shown verbatim with no annotation so the operator can make their own call; braid only ever produces `single`, `DUP`, and `RAID1`. |

The whole `Profile:` section is omitted when the pool is `not mounted` or
when `btrfs filesystem df` failed.

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

Disk states (compact `Drives:` list and detail view):

| State | Meaning |
|---|---|
| **present** | Disk is online and healthy |
| **MISSING** | Disk not found at its by-id path |
| **LUKS HEADER UNREADABLE** | Device present but LUKS header cannot be read |
| **LUKS HEADER DAMAGED** | Device present but LUKS header is damaged |
| **LUKS UUID MISMATCH** | Device present but its LUKS header UUID differs from the recorded member -- swapped, cloned, or reformatted; run `braid doctor` |
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

#### Pending LUKS header backups

When a header-mutating operation
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

**ENOSPC risk on RAID1 pool.** When an intact mounted pool is one
disk-loss away from insufficient RAID1 chunk-pair space, `braid status`
prints:

```
warning: ENOSPC risk: 1 of 3 devices have less than 1.00 GiB unallocated -- pool may be unable to allocate new RAID1 chunks. Free up files or run 'btrfs balance start -dusage=0 -musage=0 <mount>' to reclaim empty chunks.
```

For 2-disk pools, the warning fires when either device drops below the
threshold because new RAID1 chunks need space on both devices. For 3+
device pools, braid simulates each possible single-disk loss and warns
when any survivor set would have too little pairable unallocated space.
The per-device threshold is `min(1 GiB, 10% of total device bytes)`,
matching btrfs's effective data chunk size.

See [Balance fails with No space left on device](../guides/troubleshooting.md#balance-fails-with-no-space-left-on-device)
for recovery options.

## JSON output

`--json` produces a structured report suitable for monitoring tools. Key fields:

- `status`: `"intact"`, `"degraded"`, or `"not_mounted"`
- `disks`: array of per-disk reports -- one element per disk braid knows
  about: present pool members (matched members and foreign live devices),
  plus configured disks that are not currently live pool members (reported
  as `missing`, `unknown`, or `luks-header-*`; see the `status` values
  below). The field list below describes a **present** element (as in the
  example); non-present elements differ as called out per field and in the
  note after the example.
  - `luks_uuid`: the disk's **live-observed** LUKS UUID -- the persistent
    member identity. For a matched present member it equals the `pool.json`
    membership key; a foreign present device carries a live UUID that is
    **not** in membership (paralleling its mapper-basename `name`).
    **Populated for present disks only.** A non-present disk reports `""`,
    because the UUID is read from the live device and is unavailable when
    the device is absent; correlate non-present disks by `name`, not
    `luks_uuid`.
  - `name`: operator-facing name (e.g. `toshiba1`). For a matched present
    member it is resolved via the UUID-keyed membership join; for a foreign
    present device it falls back to the mapper basename; for a non-present
    disk it is the configured name. For display/command selection, not
    identity.
  - `by_id`: stable `/dev/disk/by-id/...` hardware path -- a runtime
    handle, not identity.
  - `mapper`: the **observed** device-mapper name -- normally
    `braid-<name>`, but may differ when a member is open under a drifted
    mapper (decision 024 tolerates mapper drift). A runtime handle, not
    identity; do not reconstruct it as `braid-${name}`.
  - `underlying`: current backing block device (e.g. `/dev/sda`), or
    `null` when the disk is not present.
  - `devid`: btrfs device ID **as a number** (e.g. `1`), or `null`
    when the disk is not a live pool member.
  - `status`: one of `present`, `missing`, `luks-header-unreadable`,
    `luks-header-damaged`, `unknown`.
  - `errors`: btrfs I/O error counters (`read`, `write`, `flush`,
    `corruption`, `generation`, all integers). Present when btrfs device
    stats are available; omitted entirely otherwise -- including for
    present disks when `btrfs device stats` fails (which also emits a
    `btrfs device stats failed` advisory).

```json
{
  "name": "toshiba1",
  "mapper": "braid-toshiba1",
  "by_id": "/dev/disk/by-id/ata-TOSHIBA_MN07ACA12T_1234",
  "luks_uuid": "aaaaaaaa-1111-2222-3333-444444444444",
  "devid": 1,
  "underlying": "/dev/sda",
  "status": "present",
  "errors": { "read": 0, "write": 0, "flush": 0, "corruption": 0, "generation": 0 }
}
```

> A non-present disk (`missing`, `unknown`, or `luks-header-*`) reports
> `"luks_uuid": ""`, `"devid": null`, `"underlying": null`, and no
> `errors` key. Correlate it by `name`.

- `alert_active`: boolean
- `alert_causes`: array of alert cause objects. **Omitted entirely when no
  alert is active** (the key is absent, not `[]`) -- check the
  always-present `alert_active` boolean first, mirroring how `advisories`
  is "omitted when none". When present, each object is tagged by a `type`
  discriminator:
  - `{ "type": "btrfs_device_errors", "devid": <number> }` -- btrfs I/O
    errors on that device.
  - `{ "type": "missing_device", "devid": <number> }` -- a device counted
    as missing.
  - `{ "type": "smartd_alert" }` -- a SMART health warning from smartd.
  - `{ "type": "computation_error", "detail": "<string>" }` -- braid could
    not compute alert state; `detail` explains.
- `advisories`: array of human-readable advisory strings (omitted when
  none). See the Advisories section above for what currently produces
  them.
- `missing_devids`: array of every devid counted in `missing_count`
  (btrfs-MISSING devices and null-underlying mappers whose backing device has
  disappeared). For destructive `remove-missing` / `replace --missing-id`
  workflows, see those commands' notes -- a null-underlying devid here will be
  rejected by those commands until btrfs promotes it to MISSING.
- `profile`: object with `data`, `metadata`, and `system` arrays, present
  whenever btrfs reports block-group allocation and omitted when the pool is
  not mounted or `btrfs filesystem df` failed. Each array contains raw btrfs
  profile names such as `single`, `DUP`, `RAID0`, `RAID1`, `RAID1C3`,
  `RAID1C4`, `RAID5`, `RAID6`, `RAID10`, or an unrecognized name verbatim.
  Arrays use canonical domain order, not alphabetical order, so mixed data is
  `["single", "RAID1"]`, not `["RAID1", "single"]`. An empty array means btrfs
  reported no block groups of that type.
- `capacity`: `total_bytes`, `used_bytes`, `free_bytes`
- `allocation`: array of block-group entries, one per allocated type.
  Each entry has `bg_type` (e.g. `Data`, `Metadata`, `System`), `profile`
  (raw btrfs profile name, same vocabulary as `profile` above),
  `used_bytes`, and `allocated_bytes` (both integers). Omitted when the
  pool is not mounted or `btrfs filesystem df` failed.

3-disk RAID1 profile:

```json
"profile": {
  "data": ["RAID1"],
  "metadata": ["RAID1"],
  "system": ["RAID1"]
}
```

Single-disk bootstrap profile:

```json
"profile": {
  "data": ["single"],
  "metadata": ["DUP"],
  "system": ["DUP"]
}
```

Mixed data after interrupted balance:

```json
"profile": {
  "data": ["single", "RAID1"],
  "metadata": ["RAID1"],
  "system": ["RAID1"]
}
```

The human-facing redundancy annotations from the text output, such as
`(no redundancy)`, `(same-disk copies; no disk redundancy)`, and
`(not fully redundant)`, do not appear in JSON. The JSON payload carries only
the btrfs profile names braid observed; consumers apply their own policy.
- `balance`: state object (`idle`, `running`, `paused`, `unknown`)
- `last_scrub`: state object (`never`, `running`, `finished`, `aborted`,
  `interrupted`, `unknown`). For `finished`, `aborted`, and `interrupted`,
  `started_at` is an offset-free host-local ISO-8601 wall-clock timestamp
  (`YYYY-MM-DDTHH:MM:SS`) as reported by btrfs. It records `Scrub started`, or
  `Scrub resumed` after a resumed scrub, and is not directly comparable to UTC
  fields such as pending-operation `started_at` values ending in `Z`.

## Related commands

- [braid unlock](unlock.md) -- bring the pool online
- [braid replace](replace.md) -- repair a degraded pool
- [braid remove-missing](remove-missing.md) -- forget a dead device
  (operates only on btrfs-authoritative MISSING devids; see that command's note
  on transient null-underlying state)
- [braid idle](idle.md) -- machine-friendly idle/busy check for autosuspend
