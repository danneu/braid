# Persist `acked-stats.json` on offline ack of latched alerts

## Context

`cli/src/ack.rs:71-89` (`ack_offline`) only deletes `alert-latch.json`, the
`.corrupt` sidecar, and the smartd flag file. It never updates
`/var/lib/braid/acked-stats.json`. This breaks the latched-until-ack invariant
from `docs/decisions/014-alerts.md`:

- `MissingDevice { devid }`: still-missing devid re-fires on next mount because
  `compute_alert_state` checks `acked.0.get(&key).map(|d| d.missing_acked).unwrap_or(false)`
  (`cli/src/alert.rs:113-118`) and the entry is still absent.
- `BtrfsDeviceErrors { devid }`: non-zero counters re-fire because
  `has_new_errors` compares against `AckedDeviceCounters::default()` (zeros).
- `SmartdAlert` is correctly silenced because the flag file is the
  authoritative trigger source.
- `ComputationError` re-fires only if the underlying issue persists (correct).

User-visible symptom: "I just acked. Why did it come back?" after lock -> ack
-> unlock -> mount cycle. The cited test `tests/cli/braid-monitor.py:131-146`
("Btrfs alert latched after pool offline") never remounts after offline ack,
so the regression is uncovered.

The fix shape is **asymmetric** (per user choice): snapshot `MissingDevice`
into `acked-stats.json` from the latch, refuse offline ack when the latch
carries any `BtrfsDeviceErrors` cause (we cannot capture the counter baseline
without a mounted pool). The existing online `cmd_ack` path is unaffected
since it has live `device_stats` and uses `snapshot_current`.

## Critical files

- `cli/src/ack.rs` -- `cmd_ack` and `ack_offline`. The behavioral change lives here.
- `cli/src/alert.rs` -- adds a small helper for the additive `missing_acked` update.
- `tests/cli/braid-monitor.py` -- existing offline-ack subtest extended;
  new BtrfsDeviceErrors-refusal subtest added.
- `docs/decisions/014-alerts.md` -- one paragraph added documenting the
  offline-ack policy (asymmetric, why) so the invariant text stays consistent.

## Approach

### 1. Carry the parsed latch into `ack_offline`

`cmd_ack` already loads the latch at `cli/src/ack.rs:17-24` to derive
`latch_count` and `latch_corrupt`. Refactor that block to keep the parsed
`AlertState` so `ack_offline` can inspect causes:

```rust
let (latch_state, latch_corrupt) = match alert::load_alert_latch(paths) {
    Ok(Some(s)) => (Some(s), false),
    Ok(None) => (None, false),
    Err(e) => {
        eprintln!("warning: alert latch unreadable -- acknowledging anyway: {e}");
        (None, true)
    }
};
let latch_count = latch_state.as_ref().map(|s| s.causes.len()).unwrap_or(0);
```

Pass `latch_state` (and keep `latch_count`/`latch_corrupt`) into
`ack_offline`. The online path at `cli/src/ack.rs:50-51` is untouched -- it
keeps using `snapshot_current` against live `device_stats`.

### 2. Refuse offline ack if the latch contains `BtrfsDeviceErrors`

In `ack_offline`, after the existing `has_alert` gate, check for any
`AlertCause::BtrfsDeviceErrors { .. }` in `latch_state`. If present, return a
new `AckError` variant with an actionable message:

```
cannot ack btrfs device errors while pool is offline -- unlock the pool first
```

Use `--` (double hyphen) per CLI Output Style. Add the variant to the
`AckError` enum at `cli/src/ack.rs:100-112`.

The check intentionally refuses the *whole* ack rather than partial-acking
the other causes. Partial ack would leave the user in an ambiguous "I acked
but it still says ALERT" state, which is worse UX than a single clear "go
unlock first" failure.

### 3. Snapshot `MissingDevice` causes into `acked-stats.json`

When the latch is parseable and the BtrfsDeviceErrors check passes, walk
`latch_state.causes` and collect the set of latched `MissingDevice` devids.

**Only touch `acked-stats.json` if there is at least one `MissingDevice`
cause.** A parseable latch with only `SmartdAlert` and/or `ComputationError`
causes does not need ack-state updates -- those causes are silenced via the
flag file and natural absence respectively. Coupling the load/save to
`acked-stats.json` for those latches would let an unrelated corrupt ack file
fail an otherwise-fine offline ack. Pseudocode:

```rust
let missing_devids: Vec<u64> = latch_state
    .as_ref()
    .map(|s| s.causes.iter().filter_map(|c| match c {
        AlertCause::MissingDevice { devid } => Some(*devid),
        _ => None,
    }).collect())
    .unwrap_or_default();

if !missing_devids.is_empty() {
    let mut acked = alert::load_acked_stats_fallible(paths)?;
    for devid in &missing_devids {
        alert::mark_missing_acked(&mut acked, *devid);
    }
    alert::save_acked_stats(&acked, paths)?;
}
```

**Use the fallible loader, not the lossy detector loader.**
`load_acked_stats` (`cli/src/alert.rs:58-68`) silently treats read AND parse
errors as empty -- correct for detector paths but wrong for a mutation that
will rewrite the file. Use `load_acked_stats_fallible`
(`cli/src/alert.rs:222-232`), which only treats `NotFound` as empty and
propagates read/parse errors; this matches the policy used by
`drop_ghost_acked_for_devids` and is documented in its doc comment.

`load_acked_stats_fallible` is currently private. Promote to `pub(crate)`
(or `pub` -- it's already used internally only) so `ack.rs` can call it.
Errors flow into `AckError::Io` (`cli/src/ack.rs:110`), which already
propagates `std::io::Error`. Result: a corrupt `acked-stats.json` causes
`braid ack` to fail loud rather than silently overwriting the file --
**only when** the latch actually contains a `MissingDevice` cause.

Add a small helper in `cli/src/alert.rs` next to `snapshot_current` to keep
the entry-shape logic colocated with the other AckedStats mutators:

```rust
/// Mark a devid as missing-acked in acked-stats. Inserts an entry with
/// default device_stats if absent, preserves any existing device_stats
/// baseline if present.
pub fn mark_missing_acked(acked: &mut AckedStats, devid: u64) {
    acked
        .0
        .entry(devid.to_string())
        .and_modify(|d| d.missing_acked = true)
        .or_insert(AckedDisk {
            missing_acked: true,
            device_stats: AckedDeviceCounters::default(),
        });
}
```

This is the same insert-or-update shape `snapshot_current` already uses for
missing devids at `cli/src/alert.rs:204-212`; pulling it into a named helper
prevents the offline-ack call site from duplicating that JSON-shape decision.

### 4. Corrupt-latch behavior preserved

When `latch_state` is `None` and `latch_corrupt` is `true`, we cannot extract
causes. Keep the existing behavior: succeed (delete files) so the operator can
clear the corrupt file with the pool offline. This is pinned by
`tests/cli/braid-monitor.py:157-166` ("Corrupt latch (offline): ack clears
it without PoolNotMounted"). The new code path runs only when
`latch_state.is_some()`, so the existing test continues to pass without
modification.

### 5. Reconcile interaction (sanity check, no code change)

`reconcile_acked_stats` at `cli/src/alert.rs:270-291` runs at the start of
each monitor cycle. After the fix, when a previously-missing devid returns
to the pool, reconcile will reset `missing_acked` to `false` for that devid
(line 284-287). This is the existing self-heal path and matches the ADR 014
"resets missing_acked for now-present devids after drive replacement" line.
The fix slots in without touching reconcile.

### 6. Documentation

Add one short paragraph under `docs/decisions/014-alerts.md` -> "Latched
alerts" or "Corrupt latch recovery" describing the offline-ack policy:

> Offline `braid ack` (pool not mounted) updates `acked-stats.json` from
> the latch's `MissingDevice` causes (sets `missing_acked = true` on each
> latched devid). It refuses if any `BtrfsDeviceErrors` cause is present,
> because the counter baseline requires live `btrfs device stats` output.
> The user is told to unlock first.

### 7. Tests

**Extend** `tests/cli/braid-monitor.py:131-146` ("Btrfs alert latched after
pool offline" -- misnamed; it actually exercises a `MissingDevice` latch):

After `braid ack` succeeds offline (line 145), assert `acked-stats.json`
exists and contains `missing_acked: true` for the closed disk's devid.
Then re-unlock both closed disks, remount degraded, run `braid monitor`,
assert exit 0 and no latch file. This is the actual re-fire regression
gate and is the one assertion that catches the bug today.

Optionally rename the subtest to "MissingDevice alert acked offline does
not re-fire on remount" for accuracy. The legacy name implies BtrfsDeviceErrors.

**Add** a new subtest in the same file: "Offline ack refused when latch
contains btrfs device errors -- mixed-cause atomicity":

The latch must be a *mixed* fixture (BtrfsDeviceErrors + MissingDevice) so
the test enforces the all-or-nothing contract. A BtrfsDeviceErrors-only
latch would not catch a regression that applies the `MissingDevice` ack
update *before* checking for BtrfsDeviceErrors.

**Sensitive baseline.** The preceding subtest leaves `acked-stats.json`
with `missing_acked: true` for the closed disk's devid. If the new
fixture's `MissingDevice` cause names that same devid, a buggy
partial-apply implementation would leave `acked-stats.json` byte-identical
(the entry is already `true`), and the test would silently pass. To make
the assertion sensitive: `rm -f /var/lib/braid/acked-stats.json` before
writing the latch fixture, then assert the file is still absent after the
refusal. (Alternative: seed `acked-stats.json` with a known entry whose
`missing_acked: false` would visibly flip on partial-apply; the rm
approach is simpler and equally diagnostic.)

- Reset acked-stats: `rm -f /var/lib/braid/acked-stats.json`.
- Hand-write the latch fixture at `/var/lib/braid/alert-latch.json` using
  the full `AlertState` JSON shape (`load_alert_latch` deserializes
  `AlertState`, not a bare `AlertCause`):

  ```json
  {
    "active": true,
    "causes": [
      {"type": "btrfs_device_errors", "devid": 1},
      {"type": "missing_device", "devid": 2}
    ]
  }
  ```

  The `type` tag values match the `#[serde(tag = "type", rename_all = "snake_case")]`
  attribute at `cli/src/alert.rs:21`. Writing only the cause object would
  exercise the corrupt-latch path (parse failure on missing `active`/`causes`
  fields), not the refusal path -- which is exactly what we are NOT testing.
- Lock the pool (`umount`, `cryptsetup close ...`).
- Run `braid ack`; assert non-zero exit and stderr contains "unlock the
  pool first".
- Assert `alert-latch.json` is still present and byte-identical to the
  written fixture (refusal must not delete or rewrite it).
- Assert `acked-stats.json` is still absent (refusal must not partially
  apply the `MissingDevice` update -- this is the mixed-cause regression
  gate, and absent-vs-present is the most diagnostic baseline).
- Unlock + remount degraded, run `braid ack` (online); assert it succeeds.

This pins (a) the refusal contract, (b) all-or-nothing atomicity for the
mixed-cause case, and (c) the "online ack still works after offline
refusal" property -- without needing to inject real btrfs device-stat
counters (which the codebase has no existing pattern for, per the project
exploration).

**Unit tests** in `cli/src/ack.rs` (new `#[cfg(test)] mod tests`) or as
`#[test]` blocks reusing the helper at `cli/src/alert.rs:415` style:

- `ack_offline_with_missing_device_cause_marks_missing_acked` -- prepare a
  state dir with a latch containing `MissingDevice { devid: 2 }`, call
  `ack_offline`, assert `load_acked_stats` returns an entry with
  `missing_acked: true` for devid 2 and the latch file is gone.
- `ack_offline_refuses_when_btrfs_errors_mixed_with_missing` -- prepare a
  state dir with a latch containing both `BtrfsDeviceErrors { devid: 1 }`
  and `MissingDevice { devid: 2 }`. **Start with `acked-stats.json`
  absent** (sensitive baseline -- partial-apply would create the file).
  Assert `ack_offline` returns the new `AckError` variant, the latch file
  is byte-identical to what was written, and `acked-stats.json` is still
  absent (no partial application).
- `ack_offline_preserves_existing_device_stats_baseline` -- seed
  `acked-stats.json` with `{1: {missing_acked: false, device_stats: {read_io_errs: 7, ...}}}`,
  latch contains `MissingDevice { devid: 1 }`, call `ack_offline`, assert
  the resulting entry has `missing_acked: true` AND `device_stats.read_io_errs == 7`
  (the helper must preserve, not zero, the baseline).
- `ack_offline_corrupt_latch_still_clears_files` -- existing behavior
  regression gate; no acked-stats change, just file cleanup.
- `ack_offline_corrupt_acked_stats_propagates_io_error_when_missing_cause` --
  seed `acked-stats.json` with non-JSON bytes, latch contains
  `MissingDevice { devid: 1 }`. Assert `ack_offline` returns
  `AckError::Io` and the corrupt `acked-stats.json` is byte-identical
  (mutation path must fail closed, not silently overwrite -- this pins
  the use of the fallible loader).
- `ack_offline_smartd_only_latch_does_not_load_acked_stats` -- seed
  `acked-stats.json` with non-JSON bytes, latch contains only
  `SmartdAlert` (parseable, no `MissingDevice` and no `BtrfsDeviceErrors`),
  and the smartd flag file exists. Assert `ack_offline` returns `Ok(())`,
  the smartd flag is removed, the latch is removed, and the corrupt
  `acked-stats.json` is byte-identical -- pinning the gate that skips the
  acked-stats load when no `MissingDevice` cause is present. A symmetric
  test using a `ComputationError`-only latch is also worth adding so the
  gate is pinned against both non-Missing cause types.

Each block-comment uses the project's three-section format (Intent / Why
it exists / Scenario) per `tests/` convention.

Note that `ack_offline` is currently private. Either change it to
`pub(crate)` for direct unit testing, or test through `cmd_ack` with a
`MockRunner` that returns `ProbeError::NotBtrfs` (matches `cli/src/ack.rs:29`).
The latter is preferred -- it tests the whole offline path including the
`probe_pool` -> `ack_offline` dispatch -- and matches how monitor tests
exercise via public API.

## Verification

1. **Rust unit tests**: `just test-rust`. New ack.rs tests must pass; the
   existing `cli/src/alert.rs` tests for `snapshot_current`,
   `reconcile_acked_stats`, and `merge_into_latch` must still pass
   unchanged (no schema change, no logic change to those paths).

2. **VM test**: `just test-vm braid-monitor` (or whatever check name is
   wired in `flake.nix` for `tests/cli/braid-monitor.nix`). Both the
   extended subtest and the new BtrfsDeviceErrors-refusal subtest must
   pass.

3. **End-to-end manual check** (optional, on a 2-disk dev VM):
   - Build pool, close one mapper, mount degraded.
   - `braid monitor` -> exit 1, latch present.
   - Lock the pool (umount + cryptsetup close).
   - `braid ack` -> should succeed and write `missing_acked: true` to
     `acked-stats.json`.
   - Re-open mappers, mount degraded.
   - `braid monitor` -> must exit 0 with no new latch (the regression).

4. **Refusal manual check**:
   - Hand-write a `BtrfsDeviceErrors`-only latch JSON, lock the pool.
   - `braid ack` -> must fail with the "unlock the pool first" message,
     latch preserved, `acked-stats.json` not silently created.

5. **Parser canary** (`just test-parsers`) is not affected -- no
   tool-output parsing changes.

## Out of scope

- Counter-snapshot-in-latch (Option B from review) -- rejected in favor of
  asymmetric simplicity. Revisit only if operators report the offline
  refusal is a real workflow burden.
- Changing the latch JSON schema. The existing `AlertCause` shape is
  preserved exactly; only `ack_offline` semantics change.
- Renaming `acked-stats.json` or splitting it. ADR 014's "ack state
  separate from pool.json, machine-local" stays intact.
