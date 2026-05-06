# Snapshot smartd handling in `cmd_ack_impl` (gate + cleanup)

## Context

`cmd_ack_impl` (`cli/src/ack.rs`) reads two independent pieces of alert
state during a single ack invocation, but on different timestamps:

- `alert::load_alert_latch(paths)` -- snapshotted **once** at line 31 into
  `latch_state` / `latch_corrupt`, then carried through the function.
- `alert::smartd_alert_active(paths)` -- read **separately** at line 51
  (mounted gate) and at line 89 (offline gate inside `ack_offline`); the
  cleanup helper at line 157 also unconditionally calls
  `remove_smartd_alert_flag` regardless of what the snapshot saw.

`probe_pool` runs between the latch read and the smartd reads, taking
measurable wall-clock time across multiple shell-outs (`btrfs filesystem
show` + `cryptsetup status`/`luksUUID` per disk). The smartd hook
(`modules/braid/monitor.nix:23-26`) is a bare unlocked `touch` that can
fire any time. Per ADR 014, `monitor`/`ack`/`add`/`remove`/`remove-missing`
are serialized by `/run/braid-pool.lock`, but the smartd hook is **not**
under that lock -- the race is real.

The mismatch produces two related bugs of the same shape:

1. **Gate window** -- when the latch was empty at snapshot, a smartd flag
   that arrives during `probe_pool` flips the gate from "no alerts" to
   "ack proceeds": offline ack returns `Ok` instead of `PoolNotMounted`,
   mounted ack runs the full path. Either way `cleanup_alert_files_and_beeper`
   then deletes the freshly-arrived flag, suppressing the next monitor
   cycle's smartd cause.
2. **Cleanup window** -- when the snapshot already had a non-smartd cause
   (e.g. `BtrfsDeviceErrors`, `MissingDevice`), ack proceeds for that
   cause; if a smartd flag arrives during `probe_pool` (or anywhere up to
   cleanup) it is still deleted by the unconditional
   `remove_smartd_alert_flag` call. Same "unobserved smartd alert
   swallowed" symptom, on the existing-alert path.

Severity is Low (UX inconsistency, not data or safety) but the fix is
small. Outcome: every gating *and cleanup* decision in a single ack
references a single coherent snapshot of `(latch_state, latch_corrupt,
smartd_active)`. A smartd hook firing after the snapshot is consumed by
this ack only when the snapshot already represented an active smartd
source -- the flag was present at entry, **or** the latch carried a
`SmartdAlert` cause (the crash-recovery exception, see Section 2 below).
Otherwise the late flag is left for the next ack/monitor cycle.

## The fix

Three coordinated changes in `cli/src/ack.rs`, plus a one-paragraph ADR
update.

### 1. Snapshot `smartd_active` at the top of `cmd_ack_impl`

Add one read right after the latch snapshot (between `latch_count`
derivation and the `probe_pool` call):

```rust
let smartd_active = alert::smartd_alert_active(paths);
```

Update the comment that introduces the latch read (current lines 27-30) so
it covers both inputs and explains why both are pre-probe:

> Snapshot the gating inputs (alert latch + smartd flag) before probing
> the pool. Both feed the "is there an alert?" decision and the
> snapshot-scoped cleanup decision. `probe_pool` is slow enough (multiple
> per-disk shell-outs) for the asynchronous smartd hook to fire during it;
> reading smartd after the probe would let a hook firing during the probe
> either flip an empty-latch gate or get swallowed by cleanup. An
> unreadable latch counts as active for gating so the user can clear a
> corrupt file even with the pool offline.

Delete the local `let smartd_active = alert::smartd_alert_active(paths);`
at line 51 -- with the top-level snapshot in scope, the early-return
condition at line 52 already references the right name.

### 2. Snapshot-scoped cleanup helper

`cleanup_alert_files_and_beeper` currently removes the smartd flag
unconditionally. Add a `remove_smartd: bool` parameter and gate the call:

```rust
fn cleanup_alert_files_and_beeper(
    paths: &StatePaths,
    stop_beeper: &dyn Fn(),
    remove_smartd: bool,
) -> Result<(), std::io::Error> {
    if remove_smartd {
        alert::remove_smartd_alert_flag(paths)?;
    }
    alert::remove_alert_latch(paths)?;
    alert::remove_alert_latch_corrupt(paths)?;
    stop_beeper();
    Ok(())
}
```

Update the docstring above it to call out the snapshot scope contract:
"Callers compute `remove_smartd` as `smartd_active || latch_had_smartd`
from inputs snapshotted at entry. Cleanup deletes the smartd flag only
when the snapshot already represented an active smartd source -- the
flag was present at entry, or the latch carried a `SmartdAlert` cause
(the crash-recovery exception). A flag that arrives after a snapshot
with neither condition is left for the next monitor cycle."

Both call sites compute `remove_smartd` the same way:

```rust
let latch_had_smartd = latch_state.as_ref().is_some_and(|s| {
    s.causes.iter().any(|c| matches!(c, AlertCause::SmartdAlert))
});
let remove_smartd = smartd_active || latch_had_smartd;
cleanup_alert_files_and_beeper(paths, stop_beeper, remove_smartd)?;
```

The two arms cover two distinct situations:

- **`smartd_active=true`** -- normal case. The flag was present at
  snapshot; ack acknowledges it; cleanup removes it.
- **`latch_had_smartd=true && !smartd_active`** -- crash-recovery case.
  A prior monitor cycle latched `SmartdAlert`, but by the time of this
  ack the flag is already absent (an earlier ack/cleanup partly ran, the
  flag was manually cleared, or filesystem state diverged). The user's
  `ack` invocation is still aimed at silencing the latched smartd
  source. A flag that materializes during the probe in this state is
  the same condition re-firing; cleanup removes it so the very next
  monitor cycle does not re-latch the SmartdAlert the user just acked.

When neither arm holds (the snapshot saw no smartd state at all), any
flag that exists at cleanup time arrived after the snapshot and is left
for the next monitor cycle to observe.

### 3. Thread `smartd_active` into `ack_offline`

Add `smartd_active: bool` to `ack_offline`'s parameter list (between
`latch_corrupt` and `paths`) and delete the line-89 re-read inside the
function body. Update the lone call site at line 48:

```rust
return ack_offline(latch_state, latch_corrupt, smartd_active, paths, stop_beeper);
```

`ack_offline` derives `latch_had_smartd` and `remove_smartd` itself from
the `latch_state` it already owns; no extra parameter needed beyond the
`smartd_active` bool.

### 4. Update ADR 014

Add a new subsection in `docs/decisions/014-alerts.md`, between **Latched
alerts** (line 43) and **Ack state keyed by btrfs devid** (line 45).
Suggested heading and body:

> ### Ack snapshots gating inputs before probing
>
> `cmd_ack` reads the alert latch and the smartd flag (`smartd-alert`)
> once at function entry, before `probe_pool`. Every decision in that ack
> -- the gate that decides whether to proceed and the cleanup that
> removes alert files -- references that single snapshot. The pool lock
> at `/run/braid-pool.lock` already serializes monitor vs ack vs
> add/remove writers, but the smartd hook is intentionally unlocked, so
> a per-ack snapshot is the only mechanism that gives ack a coherent view
> of smartd state.
>
> The smartd flag is cleared during cleanup when **either** the snapshot
> observed the flag active **or** the snapshot's latch carried a
> `SmartdAlert` cause. The first arm covers the normal "flag present,
> ack silences it" case. The second arm is an explicit exception for the
> crash-recovery case where a prior cycle latched `SmartdAlert` but the
> flag was already absent at snapshot (e.g. a partially-applied earlier
> ack, manual state, filesystem-level divergence) -- the user's ack is
> aimed at the latched smartd source, so a flag that the smartd hook
> writes during the probe is part of that source and is cleared.
>
> A flag that exists at cleanup time when the snapshot saw **neither**
> active smartd state **nor** a latched `SmartdAlert` cause arrived
> strictly after the snapshot and is left in place: the next monitor
> cycle is responsible for latching it cleanly.

This doesn't change the existing `Offline ack policy` section's
per-cause-type description, only documents the snapshot boundary that
governs them all.

## New tests (deterministic race coverage)

The race **is** testable: `probe_pool` reads `/proc/self/mountinfo`
through the injected `Filesystem` trait at `cli/src/probe.rs:221`
(`mount_check::fstype_at_mount_via_fs`), which is the very first thing
`probe_pool` does. A test double whose `read_to_string` writes
`paths.smartd_alert()` before returning the mountinfo content
deterministically simulates "smartd hook fired during the probe".

Two new fixtures plus six new tests in `cli/src/ack.rs`'s `mod tests`.
Each test follows the project's `// Intent / // Why it exists / //
Scenario` preamble convention (per `AGENTS.md` and `docs/testing.md`).

### Fixtures

```rust
struct OfflineFsThatTouchesSmartd<'a> {
    paths: &'a StatePaths,
}

impl Filesystem for OfflineFsThatTouchesSmartd<'_> {
    fn exists(&self, _path: &str) -> bool { false }
    fn is_block_device(&self, _path: &str) -> bool { false }
    fn read_to_string(&self, path: &str) -> Result<String, std::io::Error> {
        assert_eq!(path, "/proc/self/mountinfo");
        std::fs::write(self.paths.smartd_alert(), b"").unwrap();
        Ok(String::new())
    }
    fn list_dir(&self, _path: &str) -> Result<Vec<String>, std::io::Error> { Ok(vec![]) }
}

struct MountedFsThatTouchesSmartd<'a> {
    paths: &'a StatePaths,
}

impl Filesystem for MountedFsThatTouchesSmartd<'_> {
    fn exists(&self, _path: &str) -> bool { false }
    fn is_block_device(&self, _path: &str) -> bool { false }
    fn read_to_string(&self, path: &str) -> Result<String, std::io::Error> {
        assert_eq!(path, "/proc/self/mountinfo");
        std::fs::write(self.paths.smartd_alert(), b"").unwrap();
        Ok(MOUNTINFO_BTRFS.to_owned())
    }
    fn list_dir(&self, _path: &str) -> Result<Vec<String>, std::io::Error> { Ok(vec![]) }
}
```

### Test 1 -- offline gate, empty snapshot

`ack_offline_does_not_consume_smartd_flag_arriving_during_probe`

- Empty initial state (no latch, no smartd flag).
- `cmd_ack` with `OfflineFsThatTouchesSmartd`.
- Expect: `Err(AckError::PoolNotMounted)`; smartd flag exists at end;
  no latch created.
- Without the snapshot fix the offline branch sees `smartd_active=true`
  at the line-89 read and silently consumes the flag.

### Test 2 -- mounted gate, empty snapshot

`cmd_ack_mounted_does_not_consume_smartd_flag_arriving_during_probe`

- Empty initial state.
- `cmd_ack` with `MountedFsThatTouchesSmartd` and `mounted_probe_runner()`
  (the no-device-stats variant -- a no-op ack must not touch device stats).
- Expect: `Ok(())`; smartd flag exists at end; runner did not receive
  `BtrfsDeviceStatsJson`.
- Without the snapshot fix the gate at line 51 reads `smartd_active=true`,
  the early-return condition fails, the full ack path runs, and cleanup
  deletes the flag.

### Test 3 -- mounted cleanup, snapshot has non-smartd cause only

`cmd_ack_mounted_with_btrfs_errors_preserves_mid_probe_smartd_flag`

- Latch contains `BtrfsDeviceErrors { devid: 1 }`, no smartd flag.
- `MountedFsThatTouchesSmartd` + `mounted_probe_runner_with_device_stats()`.
- Expect: `Ok(())`; latch removed; `acked-stats.json` written; smartd flag
  exists at end.
- Without snapshot-scoped cleanup, `cleanup_alert_files_and_beeper`'s
  unconditional `remove_smartd_alert_flag` deletes the late-arrival flag
  even though the snapshot's only cause was the btrfs error.

### Test 4 -- offline cleanup, snapshot has non-smartd cause only

`ack_offline_with_missing_device_preserves_mid_probe_smartd_flag`

- Latch contains `MissingDevice { devid: 2 }`, no smartd flag.
- `cmd_ack` with `OfflineFsThatTouchesSmartd` and `PanicRunner` (offline
  path must not invoke the runner).
- Expect: `Ok(())`; latch removed; `acked-stats.json` has
  `"2".missing_acked = true`; smartd flag exists at end.
- Without snapshot-scoped cleanup, the offline cleanup path (line 137 →
  `remove_smartd_alert_flag`) deletes the late-arrival flag.

### Test 5 -- pins the `latch_had_smartd` arm on the offline path

`ack_offline_with_smartd_latch_cleans_mid_probe_smartd_flag`

- Latch contains `AlertCause::SmartdAlert`, smartd flag absent at entry.
- `cmd_ack` with `OfflineFsThatTouchesSmartd` and `PanicRunner`.
- Expect: `Ok(())`; latch removed; smartd flag **removed** at end (the
  `latch_had_smartd` arm of `remove_smartd` fires even though
  `smartd_active=false` at snapshot).

### Test 6 -- pins the `latch_had_smartd` arm on the mounted path

`cmd_ack_mounted_with_smartd_latch_cleans_mid_probe_smartd_flag`

- Latch contains `AlertCause::SmartdAlert`, smartd flag absent at entry.
- `cmd_ack` with `MountedFsThatTouchesSmartd` and
  `mounted_probe_runner_with_device_stats()` (the full ack path runs
  because `latch_count > 0`).
- Expect: `Ok(())`; latch removed; `acked-stats.json` written; smartd
  flag **removed** at end.

Tests 5 and 6 together pin the crash-recovery exception named in the
ADR update on **both** branches: the snapshot observed the latched
SmartdAlert, so a flag the smartd hook writes during the probe is the
same source re-firing and gets cleaned. Without this pair, a future
edit could simplify `remove_smartd = smartd_active` independently on
the mounted side or the offline side. Tests 1-4 all run with
`latch_had_smartd=false` and would silently keep passing under either
simplification; tests 5 and 6 are the regression gates -- the late
flag would survive the cleanup on whichever branch had its
`latch_had_smartd` arm dropped, and the corresponding test would
assert-fail.

### Existing tests must keep passing unchanged

All 14 existing `mod tests` cases in `ack.rs` exercise deterministic
fixtures where the smartd flag is either set at the start of the test
(snapshot reads `smartd_active=true`) or absent throughout (snapshot
reads `smartd_active=false`). The snapshot fix preserves behavior in both
shapes:

- `cmd_ack_noop_when_no_alerts_does_not_query_btrfs_or_write_acked_stats`,
  `cmd_ack_with_mounted_pool_and_corrupt_latch_runs_full_ack_path`,
  `cmd_ack_with_mounted_pool_and_smartd_flag_no_latch_runs_full_ack_path`
  -- mounted branch, all settle on the same gate result under the snapshot.
- `cmd_ack_with_foreign_fstype_*` (3 tests),
  `cmd_ack_impl_with_foreign_fstype_does_not_invoke_beeper`
  -- `probe_pool` errors before either gate is selected; snapshot value
  is irrelevant.
- `ack_offline_with_missing_device_cause_marks_missing_acked`,
  `ack_offline_refuses_when_btrfs_errors_mixed_with_missing`,
  `ack_offline_preserves_existing_device_stats_baseline`,
  `ack_offline_corrupt_latch_still_clears_files`,
  `ack_offline_corrupt_acked_stats_propagates_io_error_when_missing_cause`,
  `ack_offline_computation_error_only_latch_does_not_load_acked_stats`,
  `ack_offline_smartd_only_latch_does_not_load_acked_stats`
  -- offline branch, snapshot value comes from the test setup, gate and
  cleanup decisions are unchanged.
- `format_systemctl_stop_failure_*` (2 tests) -- pure formatting, untouched.

## Why not heavier alternatives

- **Wrapper struct around `(latch_state, latch_corrupt, smartd_active)`**:
  three locals are below the struct threshold; no other CLI command has a
  "snapshot all alert-state files at entry" pattern to mirror (verified
  -- `monitor.rs`, `status.rs`, `doctor.rs`, `add.rs`, `remove.rs` all
  read inline). Foreign abstraction.
- **flock around the smartd hook**: would close the residual window
  between snapshot and cleanup, but it's overkill for a Low-severity issue
  and would couple the smartd hook (a one-line `touch` today) to braid's
  runtime lifecycle. The snapshot rule already preserves the
  user-visible invariant; a flag that arrives between snapshot and
  cleanup is now correctly left for the next cycle.
- **Drop `latch_had_smartd` from the cleanup rule** (i.e. use just
  `smartd_active`): simpler but leaves a UX surprise -- if the latch
  carries `SmartdAlert` and the flag was already absent at snapshot
  (crash recovery / manual state), ack would not clean any
  freshly-arrived flag, and the next monitor cycle could re-fire the
  same SmartdAlert despite the user's ack. The disjunction matches user
  intent ("I acked the smartd alert, no smartd alerts please"); tests 5
  and 6 pin it on both branches so the simplification cannot be
  re-introduced (on either side) without a failing test.

## Verification

- `just test-rust` -- runs `cargo test`. All 14 existing ack tests plus
  the 6 new ones must pass. Pre-implementing the new tests against the
  current code is a useful sanity step: tests 1 and 2 should fail
  (offline branch silently consumes flag, mounted branch's early-return
  flips), tests 3 and 4 should also fail (cleanup deletes the flag),
  and tests 5 and 6 should pass against the current code (the
  unconditional `remove_smartd_alert_flag` in cleanup happens to give
  the same end-state as the snapshot-scoped rule's `latch_had_smartd`
  arm). After the fix, all six pass; if a future edit simplifies the
  cleanup rule by dropping `latch_had_smartd` on either branch, the
  corresponding pin (test 5 for offline, test 6 for mounted) starts
  failing.
- `cargo clippy -p braid-cli --tests` -- the
  `cleanup_alert_files_and_beeper` signature change and the
  `ack_offline` parameter add are the only signature edits.

No NixOS VM tests are affected; this is pure Rust state-handling inside
the CLI's ack command. No fixtures need refresh.

## Files modified

- `cli/src/ack.rs` -- snapshot read in `cmd_ack_impl`, comment update,
  `remove_smartd` parameter on the cleanup helper, conditional flag
  removal, `smartd_active` parameter on `ack_offline`, two new test
  fixtures (`OfflineFsThatTouchesSmartd`, `MountedFsThatTouchesSmartd`),
  six new tests.
- `docs/decisions/014-alerts.md` -- one new subsection ("Ack snapshots
  gating inputs before probing") between the existing "Latched alerts"
  and "Ack state keyed by btrfs devid" subsections.
