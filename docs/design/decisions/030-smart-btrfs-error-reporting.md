---
intent: Record why braid splits per-disk error reporting into two named concepts
  (btrfs_errors + smart), why smart is a verdict-plus-evidence object rather than
  a flat count, why status probes smartctl per disk, and why the live SMART probe
  is diagnostic-only and never feeds the alert latch. Read before changing the
  smart JSON shape, the per-disk SMART probe, or the SMART/alert boundary.
status: Active
---

# Decision: SMART + btrfs error reporting

## Context

Before this change, `braid status` reported btrfs device-error counters per disk
but nothing about SMART. The only SMART signal status surfaced was a global
smartd alert flag. The TUI showed SMART solely as a bare health enum
(`ok`/`warning`/`failing`) in a column, with no way to see *why* a drive was
degraded. SMART health was computed in `parse_smartctl` and then **discarded** --
`classify_sata`/`classify_nvme` read the underlying counters (reallocated/
pending/uncorrectable sectors; NVMe media errors, wear, spare) only to collapse
them to a `SmartHealth` enum.

These are observations from two different layers: the filesystem's own I/O
accounting (btrfs device errors) versus the drive's self-report (SMART). A degraded
drive can show clean btrfs I/O, and a drive with btrfs errors can pass SMART. They
should be surfaced as two explicitly-named concepts, not merged behind one vague
"Errors" label.

## Decision

**Two named concepts, not one merged "Errors".** The `--json` per-disk field
`errors` renames to `btrfs_errors`; a new sibling object `smart` carries the SMART
self-report. The human per-disk block relabels its `Errors:` line to `btrfs:` and
gains a parallel `SMART:` line. (braid is pre-v1.0 with no on-disk-format
backwards-compatibility obligation, so the field rename is a hard break with no
shim.)

**`smart` is a verdict plus evidence, not a flat count.** SMART's authoritative
signal is a pass/fail verdict (`health`); the counters are supporting evidence
behind it. A single summed `smart_errors` integer was rejected: it mixes units
(reallocated sectors, wear percent, media errors, spare percent are not
addable) and would render `0` on a drive reporting `passed:false` -- the exact
case where the operator most needs a signal. So `health` is the headline and the
counters are itemized beneath it.

**A `protocol` discriminator (`sata`/`nvme`).** The evidence field set differs by
transport (SATA ATA attributes vs the NVMe health-information log), so the `smart`
object is tagged by `protocol` to keep the shape unambiguous and
forward-compatible. NVMe is fully implemented, not deferred: `media_errors` is a
clean headline parallel to SATA `reallocated_sectors`, and the NVMe spare check is
a threshold *pair* (`available_spare <= available_spare_threshold`), not a generic
`> 0` rule -- a flat numeric rule would misread a healthy `available_spare` of 100.

**One threshold definition feeds all three surfaces.** `SmartEvidence::fields()`
yields each display field as `(key, value, is_concern)`; `concerns()` is its
`is_concern` subset. The verdict (`Healthy` iff `concerns().is_empty()`), the human
`SMART:` parenthetical, and the TUI evidence rows (red iff `is_concern`) all key
off this one structure and a per-field `SmartField::label()`, so the column
verdict, the human line, and the TUI rows cannot disagree on either the threshold
or the wording.

**Column-summary vs detail-evidence split.** The TUI disk-table column stays the
bare health verdict (unchanged). The error evidence lives in the per-disk detail
panel as a new `SMART` section, sibling to the existing `btrfs Device Errors`
section. `celsius` ships in the `--json` `smart` object but is not shown in the
SMART detail section (it has its own Temp column and is not a verdict input).

**`status` probes `smartctl` plainly, per disk.** Each `braid status` now spawns
one `smartctl -H -A --json` per present disk (reusing the command the TUI already
runs). No `-n standby` guard is needed: braid does only whole-system
suspend-to-RAM (no per-drive spindown; see
[power management](../../guides/power-management.md)), so whenever `status` can
run, the drives are already spinning. The probe is failure-tolerant -- any error
collapses to an `unknown` verdict -- so a flaky or absent `smartctl` never fails a
status build. This affects only the CLI `status` path, not the monitor daemon.

**Per-disk `smart` is diagnostic evidence only -- it does not feed the alert
latch.** The "SMART health warning" alert cause stays `AlertCause::SmartdAlert`,
driven by the **smartd** daemon's flag (`/var/lib/braid/smartd-alert`; see
[ADR 014](014-alerts.md)). A live `smart.health == "warning"` from the new
per-disk probe must never synthesize an `AlertCause`. So a status report can show
a degraded `smart` object while `alert_active` is `false` -- this is intentional,
and is documented so the two SMART signals (the live diagnostic probe vs the
smartd latch) are not conflated. smartd remains the single SMART alert source
because it watches continuously between status runs and applies its own vendor
thresholds; the live probe is a point-in-time diagnostic.

## Consequences

- The serialized contract grows: `SmartProbe` / `SmartHealth` / `SmartEvidence`
  are now part of the `--json` surface. The stable-only smartctl golden fixture is
  the drift canary on smartmontools bumps (virtio disks emit no SMART, so the live
  VM canary cannot exercise this path).
- `classify_sata` / `classify_nvme` / `classify_health` are removed; their
  thresholds now live once in `SmartEvidence::fields`. The verdict is derived from
  the evidence at a single call site, so the column, the human line, and the TUI
  detail cannot drift apart.
- Every `braid status` now does one synchronous `smartctl` spawn per disk. This is
  accepted per the spindown analysis above.
