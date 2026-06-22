---
experimental: false
---
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
  - scheduled scrub failed -- check journalctl -u braid-scrub.service
```

Alert causes include btrfs device errors, missing devices, SMART health warnings, and a failed scheduled scrub (`--json` cause value `{"type":"scrub_failed"}`). Alerts are latched -- they persist until acknowledged with `braid ack`, even if the underlying condition resolves.

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
  toshiba3     -    devid=3  missing
```

Each row shows the disk `name`, its short kernel device (e.g. `sda`), its
btrfs `devid`, and its state. A disk not assembled into the live pool --
missing, offline, or LUKS-mismatched -- shows `-` for its device.

The `devid` column shows `devid=N` only when the live pool currently counts
that device missing: a btrfs-MISSING device, or a hot-unplugged member whose
backing device is gone (null-underlying). It falls back to `-` when no live
devid exists -- a persisted devid the live pool no longer counts missing, or a
member with no recorded devid.

That devid is the input to the `braid remove-missing --missing-id` and
`braid replace` workflows. As with the JSON `missing_devids` field, a transient
hot-unplug devid shown here is refused by both `braid remove-missing` and
`braid replace` until btrfs promotes the device to MISSING; see
[Hot-unplug while pool is mounted](../guides/recovery-scenarios.md#hot-unplug-while-pool-is-mounted).

State values use the same vocabulary as [Per-disk detail](#per-disk-detail)
below, rendered lowercase and hyphenated in this compact list (e.g. `missing`,
`offline`, `luks-uuid-mismatch`).

### Balance progress

Shown only when a balance is running or paused:

```
Balance:  running, 3/10 chunks (30% complete)
Balance:  paused, 5/12 chunks (58% complete)
```

### Last scrub result

```
Last scrub: Mon Jan  1 00:00:00 2024 (no errors)
Last scrub: Mon Jan  1 00:00:00 2024 (3 errors)
Last scrub: Mon Jan  1 00:00:00 2024 cancelled (will resume)
Last scrub: Mon Jan  1 00:00:00 2024 interrupted
Last scrub: never
Last scrub: running (45%)
```

A nonzero error count replaces `(no errors)` with `(N errors)` on a
finished scrub, and prefixes the `cancelled (will resume)` and
`interrupted` lines when a partial scrub recorded errors. When the count
is nonzero, braid appends a copyable kernel-journal query for the
per-error detail lines:

```
Last scrub: Mon Jan  1 00:00:00 2024 (3 errors)
  scrub error details:
  sudo journalctl -k --since '2024-01-01 00:00:00' --grep 'BTRFS.*(at logical.*on (dev|mirror)|super block at physical)'
```

The `--since` argument is the scrub's start time. See
[Scrub reported errors](../guides/troubleshooting.md#scrub-reported-errors)
for how to read the journal output -- including corrected vs. uncorrectable
lines and why the count can exceed the visible journal lines.

### Per-disk detail

What each disk shows depends on whether it is a live pool member. A live pool
member shows its device path, model, serial, LUKS UUID, btrfs I/O error
counters (the `btrfs:` line), and a SMART verdict (the `SMART:` line). These
last two are different layers: `btrfs:` is the filesystem's own I/O accounting,
`SMART:` is the drive's self-report. Any other disk -- missing, offline, UUID
mismatch, header-unreadable, or unknown -- shows a reduced set: its device path
and `btrfs: unknown (<reason>)` / `SMART: unknown (<reason>)` lines in place of
counters; a UUID-mismatch disk also shows its observed `LUKS:` UUID so the
divergence is visible. Separately, any disk that needs attention -- for example
a missing disk, or a present member with nonzero error counters -- gets an
`Action:` line naming the next command (detailed below).

```
Disks:

  toshiba1          devid 1   present
    Device:  /dev/disk/by-id/ata-TOSHIBA_MN07ACA12T_1234
    Model:   TOSHIBA MN07ACA12T
    Serial:  1234ABC
    LUKS:    aaaaaaaa-1111-2222-3333-444444444444
    btrfs:   read 0 / write 0 / flush 0 / corruption 0 / generation 0
    SMART:   ok

  toshiba2          devid 2   present
    Device:  /dev/disk/by-id/ata-TOSHIBA_MN07ACA12T_5678
    Model:   TOSHIBA MN07ACA12T
    Serial:  5678DEF
    LUKS:    bbbbbbbb-1111-2222-3333-444444444444
    btrfs:   read 12 / write 0 / flush 0 / corruption 3 / generation 0
    SMART:   warning (2 reallocated)
    Action:  braid replace --old toshiba2 --new <new-name>=/dev/disk/by-id/<...>

  toshiba3          MISSING
    Device:  /dev/disk/by-id/ata-TOSHIBA_MN07ACA12T_9ABC  (not found)
    btrfs:   unknown (device absent)
    SMART:   unknown (device absent)
    Action:  braid replace --old toshiba3 --new <new-name>=/dev/disk/by-id/<...>
```

Disk states (compact `Drives:` list and detail view):

| State | Meaning |
|---|---|
| **present** | Disk is online and healthy |
| **MISSING** | Disk not found at its by-id path |
| **OFFLINE** | Disk is present and LUKS identity matches membership, but it is not assembled into the live pool |
| **LUKS HEADER UNREADABLE** | Device present but LUKS header cannot be read |
| **LUKS UUID MISMATCH** | Device present but its LUKS header UUID differs from the recorded member -- swapped, cloned, or reformatted; run `braid doctor` |
| **UNKNOWN** | State could not be determined |

**`btrfs:` line.** A live, present pool member shows real btrfs counters
(`read / write / flush / corruption / generation`). Every other disk shows
`btrfs: unknown (<reason>)`, where `<reason>` names why counters are
unavailable: `device absent`, `LUKS header unreadable`, `LUKS UUID mismatch`,
`disk offline -- not in pool`, or `metadata unavailable`. (This line was
labeled `Errors:` before braid reported SMART; it was renamed to `btrfs:` so it
reads as a sibling of the `SMART:` line, not the only error concept.)

**`SMART:` line.** A live, present pool member shows the drive's SMART verdict:
`ok`, `warning`, `failing`, or `unknown`. When the drive reports an out-of-spec
attribute, the verdict carries a parenthetical listing the concern(s) -- e.g.
`warning (2 reallocated)` or `warning (92 percentage used)`. The parenthetical
follows the evidence, not the verdict word, so a `failing` drive whose
attributes braid reads as non-nominal also lists them (`failing (5
reallocated)`); a bare `failing`/`ok`/`unknown` has no evidence to show. Every
non-present disk shows `SMART: unknown (<reason>)` with the same reasons as the
`btrfs:` line. The SMART verdict is independent of the btrfs counters: a drive
can report clean btrfs I/O while SMART reads `warning`, and vice versa.

**`Action:` line.** When a disk needs attention, `braid status` appends an
`Action:` line naming the next command, so you do not have to look it up:

| Condition | `Action:` line |
|---|---|
| Missing member, or a present member with nonzero error counts | `braid replace --old <name> --new <new-name>=/dev/disk/by-id/<...>` |
| Missing or errored device with no pool membership (foreign mapper) | `foreign mapper detected -- run 'braid doctor' to investigate` |
| LUKS UUID mismatch | `disk was swapped, cloned, or reformatted; detach the foreign disk and reattach the original, or run 'braid replace' if the swap was intentional -- run 'braid doctor' for the expected vs observed UUID` |
| LUKS header unreadable | `run 'braid doctor' for recovery guidance` |

Healthy present disks and disks in the `OFFLINE` or `UNKNOWN` state get no
`Action:` line. These hints are human-output only; `--json` consumers derive
their own remediation from the `status` and `errors` fields (the JSON
`disks[]` element has no action field).

See [braid replace](replace.md) to rebuild a missing or failing disk and
[braid doctor](doctor.md) for the guided recovery path.

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
warning: ENOSPC risk: 1 of 3 devices have less than 1.00 GiB unallocated -- if a disk fails, the pool may be unable to allocate RAID1 chunks to restore redundancy. Add capacity with 'braid add', delete unneeded files or snapshots, or compact data chunks with 'btrfs balance start -dusage=50 <mount>' (data only; do not balance metadata).
```

For 2-disk pools, the warning fires when either device drops below the
threshold because new RAID1 chunks need space on both devices. For 3+
device pools, braid simulates each possible single-disk loss and warns
when any survivor set would have too little pairable unallocated space.
The per-device threshold is `min(1 GiB, 10% of total device bytes)`,
matching btrfs's effective data chunk size.

`braid status`, `braid doctor`, and `braid monitor` share one predicate
(`evaluate_enospc_risk`), so the on-demand advisory and the proactive
`braid monitor` Warning (exit 3, no beep) never disagree about whether a
pool is at risk. Acknowledging the alert only *snoozes* the `braid monitor`
reminder; `braid status` recomputes risk live and keeps printing this
advisory for as long as the pool is at risk.

See [Balance fails with No space left on device](../guides/troubleshooting.md#balance-fails-with-no-space-left-on-device)
for recovery options.

**Config-disk probe fault.** While building per-disk detail, `braid status`
probes every configured member's LUKS header and its expected `braid-<name>`
mapper. When that probe fails -- a `braid-<name>` mapper hijacked by a foreign
container, a backing-path mismatch, a LUKS1 header, or an
unreadable/unspawnable `cryptsetup` call -- the fault is recorded as an advisory
naming the affected disk:

```
warning: disk 'disk2' mapper '/dev/mapper/braid-disk2' is open but not backed by the configured disk. Expected LUKS UUID ..., found ...
```

Unlike the mutating commands (`add`, `replace`, `enroll`), which fail closed on
such a fault, `status` is the always-available read-only diagnostic: it **stays
non-fatal (exit 0)** and still prints the full pool summary, capacity, and
per-disk detail. The fault degrades a single member, not the whole report.

A member already live and healthy in the pool keeps its `present` row -- its
identity comes from the LUKS-UUID membership join, which tolerates mapper drift
(decision 024) -- and the advisory is its only flag. **Only** an affected member
that is **not** live in the pool additionally gets an `unknown` disk row, so it
is neither silently dropped from the detail section nor mislabeled `missing` in
the compact `Drives:` list.

**Pool-membership load fault.** A missing `pool.json` is treated silently as
"no declared members," which is the expected first-boot/undiscovered state.
When `pool.json` exists but is corrupt, unreadable, or fails the load-time
uniqueness sweep, `braid status` stays non-fatal (exit 0): it records a
rebuild advisory with the underlying fault detail and the same
`discover --write` remediation used by bare `discover`:

```
warning: pool membership unreadable: ... -- run 'braid discover --write' to rebuild from existing disks (with all intended pool members attached; see docs/internals/luks-unlock.md)
```

The live mounted pool still renders from btrfs: pool summary, capacity,
scrub/balance state, and per-disk detail remain available. Because the
operator-name join is unavailable, present devices are shown under their
mapper basenames (decision 024), and configured-but-absent member enumeration
is lost until `pool.json` is rebuilt.

## JSON output

`--json` produces a structured report suitable for monitoring tools. Key fields:

- `mount_point`: the pool's configured mount path (e.g. `/mnt/storage`) -- the
  same value shown on the human-readable `Pool:` line. **Always present**, in
  both the mounted and not-mounted envelopes.
- `status`: `"intact"`, `"degraded"`, or `"not_mounted"`
- `total_devices`: total number of devices btrfs reports for the pool, as a
  number. **Present when the pool is mounted; omitted in the not-mounted
  envelope.**
- `present_count`: number of member devices currently present, equal to
  `total_devices - missing_count`, as a number. **Present when the pool is
  mounted; omitted in the not-mounted envelope.**
- `missing_count`: number of member devices counted as missing -- the
  cardinality of the `missing_devids` array below (btrfs-MISSING devices plus
  null-underlying mappers whose backing device disappeared); `0` on a healthy
  pool. **Present when the pool is mounted; omitted in the not-mounted
  envelope.**
- `fsid`: the btrfs filesystem UUID, as a string -- the same value shown on
  the human-readable `FSID:` line, and distinct from a disk's `luks_uuid`.
  **Present when the pool is mounted** (a mounted btrfs filesystem always has
  an FSID); omitted in the not-mounted envelope.
- `disks`: array of per-disk reports -- one element per disk braid knows
  about: present pool members (matched members and foreign live devices),
  plus configured disks that are not currently live pool members (reported
  as `missing`, `offline`, `unknown`, `luks-header-unreadable`, or
  `luks-uuid-mismatch`; see the `status` values below). The field list below
  describes a **live pool member** element (as in the example); diagnostic
  unpooled elements differ as called out per field and in the note after the
  example.
  - `luks_uuid`: the disk's LUKS UUID -- the persistent member identity. For a
    matched live pool member it equals the `pool.json` membership key; a
    foreign live pool device carries an observed UUID that is **not** in
    membership (paralleling its mapper-basename `name`). A
    `luks-uuid-mismatch` diagnostic row carries the observed on-disk UUID so
    the divergence is visible. Other non-live rows (`missing`, `offline`,
    `unknown`, `luks-header-unreadable`) report `""`; correlate them by
    `name`, not `luks_uuid`.
  - `name`: operator-facing name (e.g. `toshiba1`). For a matched present
    member it is resolved via the UUID-keyed membership join; for a foreign
    present device it falls back to the mapper basename; for a non-present
    disk it is the configured name. For display/command selection, not
    identity.
  - `by_id`: stable `/dev/disk/by-id/...` hardware path -- a runtime
    handle, not identity. For a matched present member it is the member's
    recorded by-id path; for a non-present disk it is the configured by-id
    path; but a foreign present device has no membership join, so it falls
    back to `/dev/mapper/<observed-mapper>` (paralleling its mapper-basename
    `name`) -- the only row whose `by_id` is not a by-id path.
  - `mapper`: device-mapper name -- a runtime handle, not identity. For a
    present pool member it is the **observed** live mapper; for a matched
    member that is normally `braid-<name>` but may have drifted (decision 024
    tolerates mapper drift), so do not reconstruct it as `braid-${name}` or you
    will miss the drift. For a non-present disk (`missing`, `offline`,
    `unknown`, `luks-header-unreadable`, `luks-uuid-mismatch`) braid does not
    report an observed mapper, so it emits the **expected** `braid-<name>`
    derived from the configured name, paralleling the configured `name` and
    `by_id` on those rows.
  - `underlying`: current backing block device (e.g. `/dev/sda`), or
    `null` when the disk is not a live pool member.
  - `devid`: btrfs device ID **as a number** (e.g. `1`), or `null`
    when the disk is not a live pool member.
  - `status`: one of `present`, `missing`, `luks-header-unreadable`,
    `luks-uuid-mismatch`, `offline`, `unknown`.
  - `btrfs_errors`: btrfs I/O error counters (`read`, `write`, `flush`,
    `corruption`, `generation`, all integers) -- the filesystem's I/O
    accounting. Present when btrfs device stats are available; omitted entirely
    otherwise -- including for present disks when `btrfs device stats` fails
    (which also emits a `btrfs device stats failed` advisory). (This field was
    named `errors` before braid reported SMART; it was renamed so it reads as a
    sibling of `smart`, not the only error concept.)
  - `smart`: the drive's own SMART self-report -- a verdict plus supporting
    evidence, a different layer from `btrfs_errors`. Present for live pool
    members; omitted for disks with no backing path to probe. The object always
    carries `health` (`"ok"`, `"warning"`, `"failing"`, or `"unknown"`). When
    SMART evidence is available it also carries a `protocol` discriminator
    (`"sata"` or `"nvme"`) and the per-protocol counters -- for SATA
    `reallocated_sectors`, `pending_sectors`, `offline_uncorrectable`; for NVMe
    `media_errors`, `critical_warning`, `percentage_used`, `available_spare`,
    `available_spare_threshold` -- plus `celsius` when the drive reports a
    current temperature. A drive whose detail log is absent (or whose health is
    `unknown`) carries `health` alone. **This field is diagnostic evidence only
    -- it does not feed the alert latch** (see the note under `alert_causes`).

```json
{
  "name": "toshiba1",
  "mapper": "braid-toshiba1",
  "by_id": "/dev/disk/by-id/ata-TOSHIBA_MN07ACA12T_1234",
  "luks_uuid": "aaaaaaaa-1111-2222-3333-444444444444",
  "devid": 1,
  "underlying": "/dev/sda",
  "status": "present",
  "btrfs_errors": { "read": 0, "write": 0, "flush": 0, "corruption": 0, "generation": 0 },
  "smart": { "health": "ok", "protocol": "sata", "reallocated_sectors": 0, "pending_sectors": 0, "offline_uncorrectable": 0, "celsius": 26 }
}
```

> A diagnostic unpooled disk (`missing`, `offline`, `unknown`, or
> `luks-header-unreadable`) reports `"luks_uuid": ""`, `"devid": null`,
> `"underlying": null`, and no `btrfs_errors` or `smart` key. `offline` is
> present but not assembled; the others reach the same blank/null row shape
> because no live member row is available. Correlate these rows by `name`.
>
> A config-disk probe fault (see the Advisories section above) on a member that
> is **not** live in the pool produces an `unknown` row of this shape -- the
> advisory carries the cause. A member that **is** live keeps its `present` row
> and is flagged by the advisory alone, so not every probe fault adds an
> `unknown` row.

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

> **The per-disk `smart` field does not feed the alert latch.** The
> `smartd_alert` cause is driven by the **smartd** daemon's flag
> (`/var/lib/braid/smartd-alert`; see [ADR 014](../design/decisions/014-alerts.md)
> and [ADR 030](../design/decisions/030-smart-btrfs-error-reporting.md)), not by
> the live per-disk SMART probe `status` runs. So a report can carry a degraded
> `smart` object (`"health": "warning"`) while `alert_active` is `false` and no
> `smartd_alert` cause is present. This is intentional: the per-disk `smart`
> field is diagnostic evidence; smartd remains the alert source.
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
  `started_at` is omitted when btrfs reported no parseable start time; the
  terminal state still comes from the `Status:` word.
  The same three states also carry `error_count` (integer) -- the count
  btrfs reported, the same number the text output renders as `(N errors)`.
  The `scrub error details:` journalctl command from the text output is
  not part of the JSON (mirroring the profile annotations above); a
  `--json` consumer derives its own `--since` value from `started_at` when
  present.

A complete report for a healthy 3-disk RAID1 pool:

```json
{
  "mount_point": "/mnt/storage",
  "status": "intact",
  "total_devices": 3,
  "present_count": 3,
  "missing_count": 0,
  "profile": {
    "data": ["RAID1"],
    "metadata": ["RAID1"],
    "system": ["RAID1"]
  },
  "fsid": "f5f5f5f5-aaaa-bbbb-cccc-d0d0d0d0d0d0",
  "capacity": {
    "total_bytes": 18000000000000,
    "used_bytes": 6000000000000,
    "free_bytes": 12000000000000
  },
  "last_scrub": {
    "state": "finished",
    "started_at": "2026-05-01T03:00:00",
    "error_count": 0
  },
  "balance": { "state": "idle" },
  "allocation": [
    { "bg_type": "Data", "profile": "RAID1", "used_bytes": 6000000000000, "allocated_bytes": 6500000000000 },
    { "bg_type": "Metadata", "profile": "RAID1", "used_bytes": 8000000000, "allocated_bytes": 9000000000 },
    { "bg_type": "System", "profile": "RAID1", "used_bytes": 65536, "allocated_bytes": 33554432 }
  ],
  "disks": [
    {
      "name": "toshiba1",
      "mapper": "braid-toshiba1",
      "by_id": "/dev/disk/by-id/ata-TOSHIBA_MN07ACA12T_1234",
      "luks_uuid": "aaaaaaaa-1111-2222-3333-444444444444",
      "devid": 1,
      "underlying": "/dev/sda",
      "status": "present",
      "btrfs_errors": { "read": 0, "write": 0, "flush": 0, "corruption": 0, "generation": 0 },
      "smart": { "health": "ok", "protocol": "sata", "reallocated_sectors": 0, "pending_sectors": 0, "offline_uncorrectable": 0, "celsius": 26 }
    }
  ],
  "alert_active": false
}
```

> When the pool is not mounted, every mounted-only field above
> (`total_devices`, `present_count`, `missing_count`, `profile`, `fsid`,
> `capacity`, `last_scrub`, `balance`, `allocation`) is omitted, leaving
> `mount_point`, `status` (`"not_mounted"`), `disks` (`[]`), and
> `alert_active`. `advisories` and `alert_causes` still follow their
> skip-when-empty rule, so a latched alert or a pending-operation advisory can
> still appear on an offline pool.

## Related commands

- [braid unlock](unlock.md) -- bring the pool online
- [braid replace](replace.md) -- repair a degraded pool
- [braid remove-missing](remove-missing.md) -- forget a dead device
  (operates only on btrfs-authoritative MISSING devids; see that command's note
  on transient null-underlying state)
- [braid doctor](doctor.md) -- diagnose pool/disk health and get recovery guidance
- [braid idle](idle.md) -- machine-friendly idle/busy check for autosuspend
