# Pivot: key the missing-device skip on devid, delete the `<missing disk>` string sentinel

## Context

`braid monitor`/`ack` alerting parses `btrfs --format json device stats`. The
parser (`cli/src/parse/btrfs_device_stats.rs:66`) classifies a row as
`DeviceStatsTarget::MissingDisk` when the btrfs `device` field is `"<missing
disk>"` or starts with `"devid:"`, and `compute_alert_state`/`snapshot_current`
use that classification to skip the missing member's stats row.

Two problems with keying on that string:

1. **It is unpinned and version/source-dependent.** `<missing disk>` is a
   *kernel* string (`reference/linux/fs/btrfs/volumes.h:821` `btrfs_dev_name()`),
   surfaced through the dev-info ioctl; btrfs-progs itself only synthesizes
   `devid:%llu` (`reference/btrfs-progs/cmds/device.c:634,688`). Whether a
   degraded mount emits `<missing disk>` or `devid:N` depends on kernel +
   btrfs-progs versions and path-canonicalization. No golden fixture or live
   capture pins it; `tests/capture-tool-fixtures.py` only ever builds a healthy
   2-disk pool.

2. **Misclassification produces a spurious alert.** If btrfs emits a third form,
   the missing row falls through to a normal path row. Its devid is in
   `recognized_devids` (because `recognized = present + missing`,
   `cli/src/probe.rs:286`), so a missing device carrying non-zero persisted
   counters would fire `BtrfsDeviceErrors` *in addition to* `MissingDevice`
   (`cli/src/alert.rs:114-127`). No test covers this; the `braid-monitor.py` VM
   test uses fresh zero-counter disks.

Commit `92faea9` already moved alert/ack/status/TUI to key on `devid` and
declared `target` "display-only metadata" -- but `alert.rs:114` and
`alert.rs:179` are the last two control-flow reads of the classification, so
that intent is not yet realized. An exploration confirmed `DeviceStatsTarget`
has **zero** non-test consumers outside those two lines: status JSON has no
target field, the TUI and `replace` pair stats to disks by `devid` via
`devid_to_name`, never by the path.

**Outcome:** stop using the btrfs-emitted string for any control flow. Key the
missing-device skip on the authoritative `missing_devids` set (already passed
in, sourced from `parse_btrfs_filesystem_show`). Then delete the now-dead
`DeviceStatsTarget` enum and the parser's sentinel branches entirely, so there
is no `<missing disk>` string left anywhere to drift or misclassify. Add a
degraded-pool golden fixture as a shape/drift canary backed by real tool output.

This matches the project Mutation-Safety heuristic: "Query the authoritative
source of state directly; do not pre-gate it with a cheaper but weaker
observable."

## Approach

### 1. Re-key the alert skip on devid (`cli/src/alert.rs`)

`compute_alert_state` (around line 103): build a `missing` set from the
`missing_devids` parameter and skip rows by devid instead of by target.

```rust
let recognized: BTreeSet<u64> = recognized_devids.iter().copied().collect();
let missing: BTreeSet<u64> = missing_devids.iter().copied().collect();

for dev in &current_stats.devices {
    if missing.contains(&dev.devid) {
        continue; // missing / null-underlying: alerts via MissingDevice, not BtrfsDeviceErrors
    }
    if !recognized.contains(&dev.devid) {
        continue; // stale identity outside current membership
    }
    ...
}
```

This suppresses `BtrfsDeviceErrors` for any missing/null-underlying devid
regardless of what string btrfs printed -- the fix for the spurious-alert
hazard, robust to all string forms.

`snapshot_current` (around line 167): **remove** the
`matches!(dev.target, MissingDisk)` skip at line 179 entirely. Snapshot every
recognized row by devid (preserving counters), then let the existing
`missing_devids` loop layer `missing_acked = true` on top via `or_default()`.

- Do **not** re-key snapshot's skip on `missing_devids` -- that would regress
  `snapshot_current_preserves_null_underlying_stats` (alert.rs:1144), which
  requires a null-underlying devid's counters to survive into the ack baseline
  so a returning device does not re-alert on old counts. Removing the skip
  preserves counters for both null-underlying and missing rows uniformly.

Update the doc comment on `compute_alert_state` (lines ~95-102) and the inline
comment in `snapshot_current` (lines ~176-178) to describe devid-based skipping;
drop the `<missing disk>` references. Remove the `DeviceStatsTarget` import
(line 6).

No `compute_alert_state`/`snapshot_current` signature changes -- `missing_devids`
is already a parameter, wired from `pool.alert_missing_devids()` in both
`monitor.rs:94` and `ack.rs:92`. The compute/snapshot passes stay agreed on
which devids matter (no ack-reconcile loop, per the regression note at
monitor.rs:354).

### 2. Delete the dead `DeviceStatsTarget` (`cli/src/parse/types.rs`, `btrfs_device_stats.rs`)

- `types.rs:308-336`: delete the `DeviceStatsTarget` enum, its `as_path()`, and
  its `Display` impl. Remove the `target` field from `DeviceErrorStats`
  (line 348) and rewrite the struct doc comment (drop the "`target` is retained
  for direct display strings" sentence). `DeviceErrorStats` /
  `BtrfsDeviceStatsOutput` derive only `Debug/Clone/PartialEq/Eq` (no serde), so
  removing the field affects no serialized output.
- `btrfs_device_stats.rs`: drop the `DeviceStatsTarget` import (line 6); remove
  the `device: String` field from `RawDeviceStatsEntry` (serde ignores the now
  unread `device` JSON key -- no `deny_unknown_fields`); delete the `target`
  computation (lines 54-70) so the map closure becomes a trivial devid+counter
  copy.

### 3. Update unit tests for the removed field

- `alert.rs`: drop `target` from the `zero_device` helper (line 509); delete the
  `zero_missing_device` helper (line 521). Rewrite the two sentinel tests
  (`missing_disk_sentinel_skipped_in_alert` ~1228,
  `missing_disk_sentinel_skipped_in_snapshot` ~1244) to construct a plain device
  for the missing devid and rely on `missing_devids` for the skip; rename them
  to reflect devid-based handling (e.g. `missing_devid_row_skipped_in_alert`).
  The null-underlying tests (1144, 1605) keep their assertions and now pass via
  the devid path.
- `btrfs_device_stats.rs`: drop the `.target.as_path()` assertions from
  `device_stats_parses_nixos_25_11_2disk` (keep devid/counter asserts); delete
  the two classification tests `device_stats_parses_observed_missing_disk_sentinel`
  (~198-240) and `device_stats_parses_upstream_devid_fallback_sentinel`
  (~242-270) -- replaced by the fixture test in step 5.
- `status.rs`: remove the `target:` field from the `DeviceErrorStats` test
  fixtures at lines 4915, 4924, 5793 (and the now-unused `DeviceStatsTarget`
  imports at 4876, 5762).
- `tui/probe.rs`: the TUI test injects a raw `{"device": "<missing disk>", ...
  "read_io_errs": 9}` row and asserts the TUI surfaces the error by devid. The
  parser now ignores `device` but still yields devid 2 + read_io_errs 9, and the
  TUI keys on devid, so the test still passes -- verify and update only the stale
  "`<missing disk>` stats rows" comment wording.

### 4. Add the linchpin regression test (`cli/src/alert.rs`)

New test proving the fix -- a missing devid carrying **non-zero** counters must
yield only `MissingDevice`, never `BtrfsDeviceErrors`:

```rust
// Intent: a missing devid's stats row never produces BtrfsDeviceErrors, even
//   with non-zero counters -- it alerts solely via MissingDevice.
// Why it exists: the skip used to key on the btrfs "<missing disk>" path
//   string; a mis-spelled/version-drifted string fell through to a path row and
//   fired a spurious BtrfsDeviceErrors on top of MissingDevice. Skipping by
//   missing_devids (devid) removes the string dependency.
// Scenario: degraded pool, devid 2 missing, btrfs reports devid 2's persisted
//   read/corruption counters as non-zero on its stats row.
let mut dev = zero_device("/dev/mapper/braid-disk2", 2);
dev.read_io_errs = 3;
dev.corruption_errs = 1;
let stats = make_stats(vec![zero_device("/dev/mapper/braid-disk1", 1), dev]);
let alert = compute_alert_state(&stats, &AckedStats::default(), &[1, 2], &[2], false);
assert_eq!(alert.causes, vec![AlertCause::MissingDevice { devid: 2 }]);
```

This fails on the pre-pivot code (the devid-2 path row would add
`BtrfsDeviceErrors { devid: 2 }`) and passes after.

### 5. Degraded-pool golden fixture + capture harness

Extend `tests/capture-tool-fixtures.py` with a degraded-mount phase. The
post-replace filesystem is still mounted after the replace captures (the
script does not `umount` until line ~321), and after the replace the live
members are `braid-vdd` + `braid-vdc`. **Unmount before rebuilding** -- `mkfs`
on a mounted/busy member fails. Insert the phase right after
`rm {MOUNT}/replacedata` (line ~317); its final `umount` subsumes the existing
teardown `umount {MOUNT}` at line ~321, after which the existing
`cryptsetup close braid-vdb` + inactive-status capture continue unchanged.
Mirror the real degraded path used by `tests/cli/braid-monitor.py:116`:

```python
# --- Degraded device stats: drop one member, capture the missing-device row ---
# Must run after the post-replace filesystem is unmounted (mkfs on a mounted
# member hits busy devices). Rebuild a clean 2-disk pool on the open mappers,
# then close one member and mount degraded.
machine.succeed(f"umount {MOUNT}")
machine.succeed("mkfs.btrfs -f -d raid1 -m raid1 /dev/mapper/braid-vdb /dev/mapper/braid-vdc")
machine.succeed(f"mount /dev/mapper/braid-vdb {MOUNT}")
machine.succeed(f"umount {MOUNT}")
machine.succeed("cryptsetup close braid-vdc")            # devid 2 now has no underlying device
machine.succeed(f"mount -o degraded /dev/mapper/braid-vdb {MOUNT}")
machine.succeed(f"btrfs --format json device stats {MOUNT}"
                f" > {FIXTURE_DIR}/btrfs-device-stats-degraded.json")
machine.succeed(f"btrfs device stats {MOUNT}"
                f" > {FIXTURE_DIR}/btrfs-device-stats-degraded.txt")
machine.succeed(f"umount {MOUNT}")
# braid-vdb stays open for the existing inactive-status capture below.
```

- Commit the captured fixtures to both lanes:
  `cli/tests/fixtures/nixos-25.11/btrfs-device-stats-degraded.json` and the
  `nixos-unstable/` mirror. The same `capture-tool-fixtures` VM script produces
  both -- `just capture-all-fixtures` (stable) and
  `just capture-all-fixtures-unstable` (re-runs it with
  `--override-input nixpkgs .../nixos-unstable`).
- The missing row's persisted counters will be zero on a fresh pool; injecting
  non-zero counters in a VM is out of scope (the step-4 synthetic test owns the
  non-zero proof). The fixture's job is to pin the **real degraded JSON shape**
  -- that the missing row still carries `devid` and the counter fields the
  parser needs, whatever string btrfs prints for `device`.
- **Validate the fixture in the shared golden harness, not inline.** Register a
  new entry in `cli/tests/support/golden_common.rs` next to the existing
  `golden_btrfs_device_stats` (line 130):

  ```rust
  golden_test!(
      golden_btrfs_device_stats_degraded,
      "btrfs-device-stats-degraded.json",
      "btrfs device stats",
      parse::btrfs_device_stats::parse_btrfs_device_stats,
      |out: parse::types::BtrfsDeviceStatsOutput| {
          // Degraded 2-disk pool: present member + missing member. The missing
          // row's `device` string varies by kernel/btrfs version; the parser
          // ignores it and keys on devid. Pin exact counts to the captured file.
          let devids: Vec<u64> = out.devices.iter().map(|d| d.devid).collect();
          assert!(devids.contains(&1) && devids.contains(&2));
      }
  );
  ```

  Both `golden_nixos_25_11.rs` and `golden_nixos_unstable.rs` `include!`
  `golden_common.rs`, so this runs in **both** lanes (stable skips a missing
  fixture; unstable, with `REQUIRE_FIXTURES = true`, panics on a missing
  fixture -- the drift canary). Placing the check inline in
  `btrfs_device_stats.rs` would only exercise `nixos-25.11` and never run under
  `just test-rust-unstable`. The non-zero spurious-alert proof stays the
  synthetic step-4 test in `alert.rs` (no fixture needed).

### 6. Update ADR 014 (architecture authority)

`docs/design/decisions/014-alerts.md` is `Active` and its header says "Read
before modifying alert computation, monitor, status, TUI alert display, or ack
semantics." AGENTS.md requires docs to track behavior changes. Today it defines
`BtrfsDeviceErrors` as "non-zero btrfs device stat counters above acked
baseline" (lines 26, 37-39) and states both `compute_alert_state` and
`snapshot_current` filter rows against `recognized_devids` (the union
*including* missing), excluding only out-of-union rows (line 66). This change
also excludes alert-local missing devids (`missing_devids` + `null_underlying`)
from `BtrfsDeviceErrors`. Update:

- The `BtrfsDeviceErrors` cause definition and the "All five counters" section
  to note the exclusion of alert-local missing devids.
- The recognized-set paragraph (line 66) to state that a row whose devid is in
  the alert-local missing set is skipped for `BtrfsDeviceErrors` (it alerts via
  `MissingDevice`), while `snapshot_current` still records that devid's counters
  by devid so a returning member does not re-alert on stale counts -- consistent
  with the offline-ack asymmetry already documented (lines 136-137).

Also grep `docs/internals/tool-behavior/device-disappearance.md` for any stale
claim that the device-stats parser classifies a `<missing disk>` / `devid:` row
into a typed target, and rewrite it to the devid-keyed model.

## Critical files

- `cli/src/alert.rs` -- the two skip sites (114, 179), helpers (509, 521), tests,
  doc comments.
- `cli/src/parse/types.rs` -- delete `DeviceStatsTarget`, drop `target` field.
- `cli/src/parse/btrfs_device_stats.rs` -- drop classification, update/replace
  tests, add fixture test.
- `cli/src/status.rs`, `cli/src/tui/probe.rs` -- test-fixture/field cleanup.
- `tests/capture-tool-fixtures.py` -- degraded capture phase (unmount-first).
- `cli/tests/support/golden_common.rs` -- new `golden_btrfs_device_stats_degraded`
  entry (runs in both lanes).
- `cli/tests/fixtures/nixos-25.11/` + `nixos-unstable/` -- new degraded fixtures
  (`.json` + `.txt`).
- `docs/design/decisions/014-alerts.md` -- document the missing-devid
  `BtrfsDeviceErrors` exclusion; plus `docs/internals/tool-behavior/device-disappearance.md`
  if it carries a stale parser-classification claim.

## Verification

1. `just test-rust` -- exercises `compute_alert_state`/`snapshot_current`, the
   updated parser unit tests, the synthetic linchpin regression in `alert.rs`,
   and the stable-lane golden tests (`golden_nixos_25_11`). This is the primary
   gate; the code change is unit-test-level. The new degraded golden entry skips
   here until step 2 captures the fixture.
2. Capture the fixtures (the golden entry reads them): `just capture-all-fixtures`
   then `just capture-all-fixtures-unstable`. Commit both
   `btrfs-device-stats-degraded.json/.txt` pairs.
3. `just test-rust` again (stable golden now consumes the 25.11 fixture) and
   `just test-rust-unstable` (`golden_nixos_unstable` requires the unstable
   fixture and parses it -- the drift canary).
4. `just test-parsers` -- CLI parser canary still green (device-stats parser
   path against live VM output).
5. Focused VM sanity (low blast radius): `just test-vm braid-monitor` to confirm
   the degraded `MissingDevice` path still fires with no new
   `BtrfsDeviceErrors`-related regression. Not a full-suite event; hand back to
   the user for any broader run.
