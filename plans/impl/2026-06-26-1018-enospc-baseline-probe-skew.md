# Plan: guard the ENOSPC baseline against the show-vs-usage probe skew

## Context

A code-review finding (Low / Correctness) flagged a TOCTOU in `cmd_monitor`'s
ENOSPC path: the `missing_count` membership gate comes from `btrfs filesystem
show` (inside `probe_pool_alerts`) while the ENOSPC `PoolKey` is built from a
separate `btrfs device usage --raw` probe (inside `evaluate_enospc_for_monitor`).
A device that drops in the gap leaves the show-derived gate reading
`missing_count == 0` while the usage-derived key already reflects the drop -- a
btrfs-missing device appears in `usage` as `(devid, device_size = 0)` (path
`<missing disk>`; parser
`cli/src/parse/btrfs_device_usage.rs#device_usage_parses_missing_device_marker`,
and braid already keys missing detection off `device_size == 0` in
`cli/src/remove_missing.rs`). `live_pool_key`
(`cli/src/alert.rs#live_pool_key`) keys on `(devid, device_size)`, so the dropped
device's key entry flips from `(devid, S)` to `(devid, 0)`.

**The first review pass concluded this was benign monitor-side and shippable as
docs-only. A second review found that conclusion is wrong, because the same skew
hits the writer:** `cmd_ack`'s `write_enospc_baseline`
(`cli/src/ack.rs#write_enospc_baseline`) has the identical two-probe structure --
`missing_count` from `show`, key from a later `usage` probe -- and it is the *only*
path that persists `enospc-ack.json`. If ack runs during the skew window on a
genuinely at-risk pool, it writes a marker keyed `(devid, 0)`. A later monitor
cycle that hits the *same* skew then builds `(devid, 0)`, **matches** the stored
marker, and -- inside the snooze window -- **suppresses** the `EnospcRisk` warning.
So the headline "the skew can only make the key differ, never match" is false as
long as a skewed marker can be persisted.

**A third review found the writer guard alone is necessary but not sufficient.**
Stopping `write_enospc_baseline` from persisting a skewed key only governs *future*
writes. A `(devid, 0)` marker already on disk -- written by the current pre-fix
code, or reaching disk by any future regression -- is still accepted by
`load_enospc_ack` and still matched by `evaluate_enospc_for_monitor`'s exact-key
branch (`cli/src/monitor.rs`: `baseline.pool_key == *live` -> `is_snoozed` ->
`None`). A later monitor cycle that hits the same skew re-derives `(devid, 0)`,
matches the stale marker, and suppresses inside the open snooze. The writer guard
closes creation; it does not neutralize what is already persisted.

**Fix -- two guards enforcing one invariant.** The invariant, widened from "ack
never writes a zero-sized key" to its full form: *a zero-sized device entry never
appears in a baseline that suppresses an alert -- never persisted, and never honored
if already present.* Two sites enforce it:

- **Writer (Change 1).** `write_enospc_baseline`, the sole on-disk writer, writes
  no marker when the fresh `usage` snapshot contains a missing/zero-sized device,
  so no new skewed marker is ever persisted.
- **Reader (Change 2).** `evaluate_enospc_for_monitor` treats a *loaded* baseline
  whose `pool_key` already contains a zero-sized device as positively invalid: it
  re-arms (removes the marker) and fires armed, exactly like the existing
  corrupt-baseline branch. Any marker left by pre-fix code self-heals on the next
  monitor cycle instead of suppressing.

Why both: the writer guard prevents *creating* garbage (you cannot snooze on an
incoherent snapshot); the reader guard guarantees garbage is never *honored* even
if it reaches disk by some other path. A writer-only fix leaves already-persisted
`(devid, 0)` markers suppressing inside their snooze window -- the gap the third
review caught.

Deliberately **not** done:

- **No monitor-side skip on a live missing device.** The reader guard (Change 2)
  keys off the *stored baseline*, never the *live usage* probe. The monitor must
  still FIRE when the live `usage` reports a zero-sized device -- and it does: a
  zero-sized device forces `margin < 0` in `evaluate_enospc_risk` (the
  `0 - threshold` term), so the cycle reaches the cause. Making the monitor instead
  *skip* on a live missing device would replace fire+clear with a one-cycle silence
  (neither `EnospcRisk` nor -- until `show` catches up next cycle -- `MissingDevice`
  fires). Change 2 invalidates a poisoned *marker* and fires; it never suppresses on
  a live probe. The two move in opposite directions and are not a contradiction.
- **No single-probe missing detector.** We do not derive `missing_count` from the
  `usage` marker wholesale (a second, parser-divergent notion of "missing" vs
  `show`'s `missing_devids`). Each guard is one narrow `device_size == 0` check.

## Change 1 (production fix): guard `write_enospc_baseline`

**File:** `cli/src/ack.rs#write_enospc_baseline`.

After the `usage` entries are parsed, before persisting the marker, bail out
(write no marker, return) if any entry is a missing device. This is the same
best-effort, write-nothing shape as the function's existing bail-outs (probe
failure, parse failure, absent FSID, pool recovered) -- log a one-line reason and
return; ack still clears the latch, and a later clean cycle baselines normally.

- **Predicate:** `device_size == 0`, btrfs's missing marker, matching the existing
  convention at `cli/src/remove_missing.rs` ("Missing devices are identified by
  `device_size == 0`"). Optional small unification: extract
  `BtrfsDeviceUsageEntry::is_missing(&self) -> bool` in `cli/src/parse/types.rs`
  (with a doc comment per repo convention) and use it both here and in
  `remove_missing.rs`. Not required for correctness; note it, don't mandate it.
- **Placement:** independent of the `!assessment.at_risk()` check -- a missing
  device means "don't baseline on this snapshot" regardless of the computed
  margin.

This is the only on-disk writer of `enospc-ack.json` (ADR 014: "Written only by
`braid ack`"; offline ack already writes no marker; the monitor only removes).
Guarding it stops any new skewed marker from being persisted; Change 2 neutralizes
markers already on disk.

## Change 2 (production fix): reject a zero-sized baseline in `evaluate_enospc_for_monitor`

**File:** `cli/src/monitor.rs#evaluate_enospc_for_monitor`.

After the baseline is loaded and unwrapped to `Some(b)` (the `None => return
Some(cause)` arm already handles "no baseline"), and *before* the `match &live_key`
comparison, bail to fire-armed-and-remove if the loaded baseline's `pool_key`
contains a zero-sized device:

```rust
let baseline = match baseline { None => return Some(cause), Some(b) => b };

// A stored key carrying btrfs's missing-marker (device_size == 0) can only come
// from a skewed write by pre-fix code (or a future regression). It is never a
// legitimate snooze -- invalidate it and fire, like the corrupt-baseline branch.
if baseline.pool_key.contains_missing_device() {
    eprintln!(
        "braid monitor: ENOSPC baseline holds a missing (zero-sized) device -- re-arming and firing"
    );
    let _ = remove_enospc_ack(paths);
    return Some(cause);
}

match &live_key { /* unchanged */ }
```

- **Why before the `match &live_key`:** the check is on the *stored* key, independent
  of the live key, so it must run regardless of which live-key arm would be taken --
  crucially the dangerous case where a skewed live key *equals* the poisoned baseline
  and the snooze is open, which otherwise hits `baseline.pool_key == *live` +
  `is_snoozed` -> `None` and suppresses. It also correctly covers the identity-gap
  (`live_key == None`) arm: a poisoned baseline is positively invalid, so removing it
  does not violate that arm's "leave a merely-uncomparable baseline in place" intent
  (which exists to protect *legitimate* keys, not poisoned ones).
- **Safe to fire here:** control has already passed `if !assessment.at_risk() {
  return None }`, so the live pool is genuinely at risk and firing `cause` is correct.
- **Predicate / helper:** add `PoolKey::contains_missing_device(&self) -> bool`
  (`self.devices.iter().any(|(_, size)| *size == 0)`) in `cli/src/alert.rs`, with a
  doc comment stating the invariant (a zero-sized entry is btrfs's missing marker and
  is never honored). Mirrors the `device_size == 0` convention of Change 1 and
  `remove_missing.rs`, and gives the monitor branch and the Change 7 test one named
  predicate. An inline `.iter().any(...)` is acceptable if a helper feels heavy.

## Change 3 (canonical docs): record the contract in ADR 014

**File:** `docs/design/decisions/014-alerts.md`, section "Severity tiers and the
ENOSPC baseline" (the owner of the `pool_key`/marker contract). Per AGENTS.md,
a behavior change to an invariant must update the ADR; this is the canonical home,
not a code comment.

Add to that section:

1. **The accepted show-vs-usage race.** `missing_count` is `show`-probed and the
   live key is `usage`-probed; a device dropping between the two probes makes the
   live `usage` key carry a `(devid, 0)` entry while `show` still reports the device
   present. Against a clean stored marker this reads as a confirmed key mismatch ->
   fire armed + re-arm (drop the marker); it self-corrects next cycle when `show`
   also sees the device missing and `MissingDevice` takes over. Never a suppressed
   `EnospcRisk`.
2. **The invariant: a zero-sized device never appears in a baseline that
   suppresses.** Enforced on both sides:
   - *Writer:* a mounted ack writes **no** marker when the fresh `usage` snapshot
     reports a missing/zero-sized device (extend the existing "writes no marker if
     the pool recovered by ack time" bullet).
   - *Reader:* the monitor treats a loaded baseline whose key already contains a
     zero-sized device as positively invalid -- re-arms and fires. So a marker left
     by older code self-heals on the next cycle and can never suppress, even when a
     concurrent skew makes the live key match it.

## Change 4 (code-comment pointers): short pointers to ADR 014

One- or two-line comments, each pointing at the ADR 014 section as the authority
(no duplicated prose):

- `cli/src/alert.rs#live_pool_key` -- note a missing device keys as `(devid, 0)`
  and the show-vs-usage skew is documented/guarded per ADR 014.
- `cli/src/alert.rs#PoolKey::contains_missing_device` -- one line: the invariant it
  enforces (a zero-sized entry is btrfs's missing marker, never a legitimate
  baseline), see ADR 014.
- `cli/src/monitor.rs#evaluate_enospc_for_monitor` -- two pointers: at the existing
  `if missing_count > 0` gate ("reconnect keeps the same key" comment), note
  `missing_count` is show-probed while the key below is usage-probed, so the skew
  fires/re-arms vs a clean baseline and never suppresses (ADR 014); and at the new
  `contains_missing_device` guard, note a zero-sized stored key is never honored
  (ADR 014).
- `cli/src/ack.rs#write_enospc_baseline` -- at the new guard: cite ADR 014 for why
  a usage-missing snapshot must not be baselined.

## Change 5 (ack regression): the writer guard is pinned

**Files:** new test in `cli/src/ack.rs` tests; fixtures in
`cli/src/test_fixtures/ack.rs`.

Model on the existing "recovered pool writes no marker" test (`ack.rs:2555+`,
`ack_mounted_probe_runner_with_healthy_enospc_usage`). Add a usage payload that is
**at-risk AND contains a `<missing disk>` / `device_size = 0` entry** (build via
`device_usage_raw_body([live_low, missing])` with `DeviceUsageSpec::missing`, or a
new `ack_btrfs_device_usage_atrisk_one_missing()` mirroring
`ack_btrfs_device_usage_atrisk` at `ack.rs:385`), served by a mounted runner whose
`show` reports both devices present (`missing_count == 0`).

- Latch carries `EnospcRisk`; run `cmd_ack_impl`.
- **Assert no marker:** `load_enospc_ack(&paths).unwrap().is_none()`. Without the
  Change 1 guard this fails (an at-risk skewed snapshot would persist a `(devid, 0)`
  marker), so the test pins the guard specifically, not just the recovered path.

## Change 6 (monitor regression): a clean baseline fires under skew, never suppresses

**Files:** new test in `cli/src/monitor.rs` tests; a `usage_2disk_one_missing()`
builder in `cli/src/test_fixtures/monitor.rs` (mirroring `usage_2disk` /
`usage_4disk_one_low`): devid 1 live at `USAGE_DEVICE_SIZE` with low unallocated
(at-risk), devid 2 `<missing disk>` at `device_size = 0`.

- Use `MonitorTestRunner::with_usage_payload(usage_2disk_one_missing())` -- the
  default `show` is already `BTRFS_SHOW_2DISK` (both present, `missing_count == 0`),
  so **leave `BTRFS_SHOW_2DISK` private**; do not reach for
  `with_usage_and_override(.., BtrfsShowPayload(BTRFS_SHOW_2DISK))`.
- Seed a clean both-present, snoozed baseline:
  `seed_enospc_baseline(matching_pool_key(), open_snooze_deadline())` -- key
  `{fsid, [(1,S),(2,S)]}` (the marker shape Change 1 permits on disk).
- Run `cmd_monitor`. **Assert the safe direction:** an `EnospcRisk` cause is present
  (`has_enospc_cause`, i.e. `Alert`, not a suppressed `Ok`) **and** the baseline was
  removed (`load_enospc_ack(&paths).unwrap().is_none()` -- the confirmed-mismatch
  re-arm).

Distinct from neighbors: `cmd_monitor_suppresses_enospc_within_snooze` proves a
*matching* key + open snooze suppresses; `cmd_monitor_stale_baseline_key_mismatch_fires_and_clears`
sources the mismatch from a *mutated baseline*. This is the only test sourcing the
mismatch from the **probe skew** (live `(devid, 0)` vs a clean baseline) **under an
open snooze**. Behavioral and structure-insensitive: asserts only on `MonitorResult`
and the on-disk marker.

## Change 7 (monitor regression): a zero-sized baseline is never honored

**Files:** new test in `cli/src/monitor.rs` tests; reuse `usage_2disk_one_missing()`
(Change 6) and add a poisoned-key builder `missing_pool_key()` in
`cli/src/test_fixtures/monitor.rs` (mirror `matching_pool_key()`, with devid 2 at
`device_size = 0`): key `{fsid, [(1,S),(2,0)]}`.

This is the test the third review asked for; it pins the reader guard (Change 2)
specifically, where Change 6 cannot -- Change 6's clean baseline mismatches the
skewed live key and fires through the *existing* confirmed-mismatch arm even without
Change 2.

- Seed a **poisoned, snoozed** baseline directly on disk:
  `seed_enospc_baseline(missing_pool_key(), open_snooze_deadline())`. This is the
  legacy marker pre-fix code could have written.
- Serve **matching skewed** live usage:
  `MonitorTestRunner::with_usage_payload(usage_2disk_one_missing())` -> live key
  `[(1,S),(2,0)]`, with the default `BTRFS_SHOW_2DISK` show (`missing_count == 0`).
  The live key now *equals* the poisoned baseline and the snooze is open.
- Run `cmd_monitor`. **Without Change 2** this hits `baseline.pool_key == *live` +
  `is_snoozed` -> `None` and **suppresses** -- the bug. **With Change 2** the
  zero-sized baseline is rejected first: assert an `EnospcRisk` cause is present
  (`has_enospc_cause`) **and** the baseline was removed
  (`load_enospc_ack(&paths).unwrap().is_none()`).

Behavioral and structure-insensitive: asserts only on `MonitorResult` and the
on-disk marker. Distinct from Change 6 (clean baseline, confirmed-mismatch arm) and
from `cmd_monitor_suppresses_enospc_within_snooze` (matching *clean* key + open
snooze legitimately suppresses) -- this is the only test where a *matching* key +
open snooze must still fire, because the matched key is poisoned.

## Verification

- `just test-rust` (or `cargo test -p braid` for the `ack` + `monitor` modules):
  existing ENOSPC ack/monitor tests stay green; Changes 5-7 pass and fail if the
  writer guard, the reader guard, or the fire-on-skew behavior regresses.
- `cargo build` + `cargo clippy` clean. Changes 1-2 are the production-behavior
  changes; Changes 3-4 are docs/comments; Changes 5-7 are tests.
- `just docs-build` (mdbook + linkcheck) for the ADR 014 edit; keep new prose and
  comments ASCII-only per repo convention.
- Manual trace re-check: confirm no path can *persist* a `device_size == 0` marker
  (mounted ack = Change 1 guarded; offline ack = no marker; monitor = remove-only)
  AND no path can *honor* one (Change 2 rejects a loaded zero-sized baseline before
  the key compare), so the invariant holds on both write and read.
