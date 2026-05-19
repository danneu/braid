# Plan: add a cleanup-pending sentinel so retry resumes ack cleanup

## Context

`AckError::CleanupFailed` (`cli/src/ack.rs:247-257`) carries this user-facing contract:

> Re-running `braid ack` after fixing the I/O issue is idempotent.

The first half of that contract was fixed in commit `c889f9c fix(ack): stop beeper before alert cleanup removals`, which moved `stop_beeper()` to the head of `cleanup_alert_files_and_beeper` (`cli/src/ack.rs:183-195`) and added regression tests. After `c889f9c`, the stop hook is invoked once on the failing first call. But the second half of the contract -- a retry that actually resumes cleanup -- is still broken.

The current cleanup order is:

```
stop_beeper
remove_smartd_alert_flag  (conditional)
remove_alert_latch
remove_alert_latch_corrupt
```

When `remove_alert_latch_corrupt` fails (the scenario pinned by `cmd_ack_returns_cleanup_failed_when_corrupt_latch_cleanup_errors_after_baseline_saved` at `cli/src/ack.rs:663-707`), `?` short-circuits but the latch JSON and smartd flag are already gone. On retry:

- **Mounted**: `load_alert_latch -> Ok(None)`, `smartd_active = false`, `latch_corrupt = false`. Early return at `cli/src/ack.rs:67-70`, "no active alerts", `Ok` -- **cleanup is never re-entered.**
- **Offline**: same input lands in `ack_offline`'s `has_alert == false` arm at `cli/src/ack.rs:114-117` and returns `Err(AckError::PoolNotMounted)` -- contradicts the recovery instruction the user just followed.

`stop_beeper` is best-effort (`cli/src/ack.rs:198-212`): `systemctl stop braid-alert.service` may exit non-zero, the function logs a stderr warning and returns. If the first call's attempt was lossy, the retry has no second chance, and the audible beep continues.

A previous draft of this plan proposed reordering removals to `corrupt -> smartd -> latch` so the latch survives on failure and the retry re-enters the full ack path. That fix breaks ADR 014's forensic-preservation guarantee (`docs/decisions/014-alerts.md:107,111`):

> the first sidecar is preserved as the highest-value forensic snapshot ... the bad bytes are preserved for forensics until an ack path that can safely clean them up

The guarantee is actively tested by `tests/cli/braid-monitor.py:208-242` ("Repeated corrupt latch preserves first sidecar"). Removing `alert-latch.json.corrupt` first means a later smartd/latch failure returns `CleanupFailed` with the forensic bytes already deleted -- losing evidence in exactly the case the operator might want to inspect.

Intended outcome: keep the destructive cleanup order unchanged (corrupt sidecar stays last so the forensic bytes are removed only after all other cleanup succeeds), and add an explicit cleanup-pending sentinel that ack writes before its fallible removals and clears only after they all succeed. The retry gate detects the sentinel and re-enters cleanup; cleanup is truly idempotent without trading away the forensic invariant.

## Approach: a cleanup-pending sentinel file

Add a new alert-state file `alert-cleanup-pending` (sibling to `alert-latch.json`, `smartd-alert`, etc.). Its presence on disk means "ack started cleanup but did not finish". `cleanup_alert_files_and_beeper` writes it at the head of the function (after `stop_beeper`) and clears it at the tail (after all `remove_*` succeed). `cmd_ack_impl` snapshots the sentinel at entry and adds a hoisted cleanup-only retry branch *before* `probe_pool_alerts`: if the sentinel is the only live signal, the branch runs cleanup directly and returns, with no probe / runner / btrfs-stats / acked-stats work. When the sentinel is set alongside another live signal, the hoisted branch is skipped and the regular ack path runs, with cleanup at the tail clearing the sentinel as usual.

```rust
fn cleanup_alert_files_and_beeper(
    paths: &StatePaths,
    stop_beeper: &dyn Fn(),
    remove_smartd: bool,
) -> Result<(), std::io::Error> {
    stop_beeper();
    alert::mark_alert_cleanup_pending(paths)?;
    if remove_smartd {
        alert::remove_smartd_alert_flag(paths)?;
    }
    alert::remove_alert_latch(paths)?;
    alert::remove_alert_latch_corrupt(paths)?;
    alert::clear_alert_cleanup_pending(paths)?;
    Ok(())
}
```

The destructive-removal order is unchanged from the current implementation: `smartd -> latch -> corrupt`. The corrupt sidecar stays last so ADR 014's forensic guarantee is preserved: any failure before the corrupt step leaves the sidecar bytes intact, and a failure at the corrupt step leaves the sidecar (the directory poison the existing test pins, or any other non-NotFound `remove_file` error) untouched.

Two invariants the new code encodes:

- **Stop-hook-attempted invariant** (already established by `c889f9c`): `stop_beeper()` runs before any fallible operation; if cleanup is entered at all, the stop hook is invoked exactly once per call. `stop_beeper` is best-effort -- it logs a warning on `systemctl` failure and returns -- so the contract is "the hook fired", not "the audible alert stopped".
- **Retry-converges invariant** (new): if `cleanup_alert_files_and_beeper` returns `Err`, the next `cmd_ack_impl` invocation has at least one active alert signal that drives it past the no-op gate so cleanup is re-entered. The signal depends on which step failed:
  - **Pre-mark failure (`mark_alert_cleanup_pending` itself returns `Err`)**: no destructive removal has run yet. The original entry signals (latch JSON, smartd flag, corrupt sidecar) are byte-identical to entry. The retry observes them through the existing `causes` / `smartd_active` / `latch_corrupt` snapshot and falls through the no-op gate -- the sentinel plays no role on this path.
  - **Post-mark failure (`mark_*` succeeded; `remove_*` or `clear_*` returned `Err`)**: the `alert-cleanup-pending` sentinel is on disk. The retry's snapshot reads it as `cleanup_pending = true` and falls through the no-op gate via that term; entry signals may or may not still be present, but the sentinel is sufficient on its own.

  Cleanup's removals are NotFound-tolerant, so the retry converges as soon as the operator has fixed whichever I/O fault originally caused the failure -- regardless of which sub-case it took.

### Sentinel helpers and accessor

- `cli/src/state_paths.rs`: add `alert_cleanup_pending()` accessor returning `root.join("alert-cleanup-pending")`.
- `cli/src/alert.rs`: add three helpers next to `remove_alert_latch_corrupt` / `smartd_alert_active`:
  - `mark_alert_cleanup_pending(paths) -> io::Result<()>` -- short-circuit `Ok(())` when `paths.alert_cleanup_pending().is_file()` is already true, so an existing marker that is already providing the retry signal never fails because of write-permission drift on a follow-up cleanup attempt. Otherwise, create the file via `OpenOptions::new().create(true).write(true).truncate(false).open(...)` and immediately drop the handle. Non-file paths (a directory, a symlink that doesn't resolve to a regular file) and other I/O errors surface as `Err` so cleanup can return `CleanupFailed` and the operator can investigate. Empty content; the file's *existence* is the signal.
  - `clear_alert_cleanup_pending(paths) -> io::Result<()>` -- `remove_file` with NotFound folded to `Ok(())`, mirroring `remove_alert_latch_corrupt`.
  - `alert_cleanup_pending(paths) -> bool` -- `paths.alert_cleanup_pending().is_file()`. Reject non-regular-file inodes (directories, etc.) so a manual poison directory at the sentinel path does not wedge the gate; the cleanup function still attempts `mark_*` and surfaces the I/O error.

The sentinel's existence-only semantic intentionally mirrors `smartd-alert`'s and avoids a JSON shape we'd have to evolve.

### Snapshot at ack entry

`cmd_ack_impl` already entry-snapshots the latch state and the smartd flag (`cli/src/ack.rs:34-48`). Add the sentinel to that snapshot:

```rust
let cleanup_pending = alert::alert_cleanup_pending(paths);
```

The sentinel-only retry runs **before** `probe_pool_alerts` (`cli/src/ack.rs:51`). A retry whose only live signal is the cleanup-pending sentinel must not depend on the pool probe succeeding -- a probe failure (mountinfo I/O error, `NotBtrfs`, etc.) would re-wedge cleanup behind a different error class than the one the operator originally tried to recover from. Cleanup operations are pure file removals plus `stop_beeper`; they don't need mount state. Hoist the branch up to the entry snapshot:

```rust
// Hoisted sentinel-only retry: runs before probe_pool_alerts.
if cleanup_pending && causes.is_empty() && !smartd_active && !latch_corrupt {
    if let Err(e) = cleanup_alert_files_and_beeper(paths, stop_beeper, false) {
        return Err(AckError::CleanupFailed(e));
    }
    println!("acknowledged current alerts");
    return Ok(());
}

// ... existing probe + mounted no-op gate unchanged ...
```

Three properties this placement encodes:

- **Probe-independent recovery**: the retry never touches `runner.run(BtrfsDeviceStatsJson)`, `probe_pool_alerts`, or anything else that could fail between the operator clearing the original I/O fault and the cleanup itself running.
- **No re-baseline**: `BtrfsDeviceStatsJson` and `save_acked_stats` are not reached. The saved baseline stays byte-identical to the first call, so any counter increments that arrived between the failed first ack and the retry still alert on the next monitor cycle.
- **Consistent message**: cleanup did real work (cleared the sentinel and any leftover files cleanup never reached on the first call), so the output is `"acknowledged current alerts"` -- matching the smartd-only and corrupt-latch success messages at `ack.rs:100`, not the true-no-op `"no active alerts"` message at `:68`.

`remove_smartd = false` is correct on this branch: `smartd_active` was false in the entry snapshot (otherwise the hoisted branch wouldn't fire), so there is no smartd flag to remove. The retry's job is the NotFound-tolerant sweep of files cleanup never reached on the first call, plus clearing the sentinel.

Because the hoisted branch filters out the sentinel-only case *before* probing, neither the no-op gate at `cli/src/ack.rs:67-70` nor `ack_offline`'s `has_alert` check at `cli/src/ack.rs:114-117` needs to mention the sentinel. The existing `causes.is_empty() && !smartd_active && !latch_corrupt` early return on the mounted side and `has_alert = !causes.is_empty() || smartd_active || latch_corrupt` check on the offline side stay byte-identical. `ack_offline`'s signature does not change.

When the sentinel is set *and* another live signal is also present (e.g. latch + sentinel after a clear-step failure where the sentinel survived but the latch was already removed -- not a real reachable state given the destructive order, but a useful invariant to confirm), the hoisted condition is false and execution falls through to the probe + full ack path as before. The probe and the full path handle the rest, and the cleanup at the tail of the full path clears the sentinel.

### `braid status` and the TUI surface the sentinel

`braid status` reads alert state via `resolve_alert_state` (`cli/src/status.rs:522-551`), which currently only consults the latch JSON and the smartd flag. A post-mark cleanup failure can leave only the sentinel on disk (latch JSON already removed in the same call); under the current `resolve_alert_state`, `braid status` would report no active alert even though `braid ack` still has cleanup work to do, contradicting the role of `status` as the alert investigation surface.

Add a third input to `resolve_alert_state`: if `alert::alert_cleanup_pending(paths)` is true, push a `ComputationError` cause onto the returned `AlertState` with a `detail` that names the condition and points at the recovery, e.g. `"ack cleanup pending -- re-run \`braid ack\` to resume"`. This mirrors the existing fail-loud pattern for unreadable latch bytes at `status.rs:527-538`. The cause is pushed regardless of whether a real `ComputationError` is already in the latch -- `merge_into_latch` collapses duplicates by `same_cause_key` and that collapse only happens in the latch-writing path (`cmd_monitor`); status is a read-only surface where the cause vector is consumed directly by callers, so an extra `ComputationError` here is harmless and informative.

The TUI uses `resolve_alert_state` through the same code path (see `cli/src/tui/`) so it inherits the new cause without further change.

### Wording: best-effort, not "silenced"

`stop_beeper` cannot prove the audible beep stopped. The plan, the rustdoc, the test names, and the failure messages all describe the contract as "the stop hook is invoked" / "the stop attempt runs", never "the beeper is silenced". The existing rustdoc paragraph at `cli/src/ack.rs:168-172` already names this caveat and stays.

### Why this over alternatives

- **Reorder cleanup so corrupt sidecar runs first (the previous draft)**: fixes the retry wedge but breaks ADR 014's forensic-preservation guarantee. The `tests/cli/braid-monitor.py:208-242` test enforces sidecar bytes survive across monitor cycles; preserving them across a cleanup-failure is the same invariant. Rejected.
- **Implicit signal -- gate on `alert_latch_corrupt().exists()` instead of an explicit sentinel**: smaller code change but overloads the corrupt sidecar's role (it is a forensic artifact, not a cleanup-pending signal) and is fragile to future cleanup-order changes. An explicit named file makes the retry semantic legible and testable.
- **Best-effort cleanup that collects all errors and always reaches the last step**: would still leak the corrupt sidecar early (whichever step is last "always runs" includes the corrupt removal), and introduces an unusual error-collection pattern that conflicts with the project's fail-loud theme.

## Critical files

- `cli/src/state_paths.rs` -- add `alert_cleanup_pending()` accessor.
- `cli/src/alert.rs` -- add `mark_alert_cleanup_pending`, `clear_alert_cleanup_pending`, `alert_cleanup_pending` helpers next to the existing latch / smartd-flag helpers. Add four Rust unit tests in `#[cfg(test)] mod tests`, following the existing style:
  - `mark_alert_cleanup_pending` creates the file when absent (regular file present after the call).
  - `mark_alert_cleanup_pending` is idempotent when the file is already a regular file (no error; file bytes unchanged).
  - `mark_alert_cleanup_pending_existing_read_only_file_does_not_require_write_permission` (`#[cfg(unix)]`): create a regular sentinel file, `set_permissions` to `0o400` (read-only), assert `mark_alert_cleanup_pending` returns `Ok(())` without trying to re-open for write. This pins the `is_file()` short-circuit and prevents a regression where the helper opens the existing marker for write and re-introduces the wedge after permission drift. Restore writable permissions before the test ends so `TempDir::drop` can clean up.
  - `alert_cleanup_pending` rejects non-regular-file inodes (`#[cfg(unix)]`): create a directory at the sentinel path, assert `alert_cleanup_pending` returns `false`. Mirrors the existing `smartd_alert_active_requires_regular_file` regression guard at `alert.rs:756`.
  - `clear_alert_cleanup_pending` is NotFound-tolerant (returns `Ok(())` when the path does not exist).
- `cli/src/ack.rs:34-50` -- entry-snapshot the sentinel via `let cleanup_pending = alert::alert_cleanup_pending(paths);` next to the existing latch / smartd snapshot. Immediately after, before the `probe_pool_alerts` call at `:51`, insert the hoisted sentinel-only retry branch: when `cleanup_pending && causes.is_empty() && !smartd_active && !latch_corrupt`, call `cleanup_alert_files_and_beeper(paths, stop_beeper, false)`, print `"acknowledged current alerts"` on success, return. On `Err`, propagate as `AckError::CleanupFailed`. The branch must not run any probe / runner / btrfs-stats / acked-stats work.
- `cli/src/ack.rs:67-70` (mounted no-op gate) -- unchanged. The sentinel-only case is already filtered out by the hoisted branch, so the existing `causes.is_empty() && !smartd_active && !latch_corrupt` check still semantically means "true no-op".
- `cli/src/ack.rs:106-117` (`ack_offline`) -- unchanged. The hoisted branch handles the sentinel-only case before the probe decides mounted vs offline, so `ack_offline` is only entered when at least one of `causes` / `smartd_active` / `latch_corrupt` is true. Its existing `has_alert` check stays byte-identical, and its signature does not change.
- `cli/src/ack.rs:183-195` (`cleanup_alert_files_and_beeper`) -- bracket the existing destructive removals with `mark_alert_cleanup_pending` (after `stop_beeper`) and `clear_alert_cleanup_pending` (after the last `remove_*`). Destructive order unchanged.
- `cli/src/ack.rs:159-182` -- update the helper's rustdoc with the new sentinel write/clear contract and the cleanup-pending-preserved invariant.
- `cli/src/ack.rs:247-257` (`AckError::CleanupFailed`) -- expand the doc: on `CleanupFailed` the stop hook has been attempted (best-effort), and the retry has a signal to drive re-execution -- either the original entry signals (when `mark_alert_cleanup_pending` itself failed before any destructive removal) or the `alert-cleanup-pending` sentinel (when `mark_*` succeeded but a later step failed). Re-running `braid ack` after fixing the I/O fault re-enters cleanup, which is NotFound-tolerant on all removals.
- `cli/src/ack.rs` `mod tests` -- add the three retry tests (mounted, offline, mounted-smartd-only). Update the existing partial-state tests at `ack.rs:663-707` and `ack.rs:976-...` with a new sentinel-exists witness; update `cmd_ack_stops_beeper_before_mounted_smartd_flag_cleanup_error` at `ack.rs:709-756` similarly.
- `cli/src/status.rs:522-551` (`resolve_alert_state`) -- consult `alert::alert_cleanup_pending` and push a `ComputationError` cause when it is true. The existing `// Alert state (latch-based)` section comment at `:513-521` should grow a sentence noting the cleanup-pending sentinel is also surfaced so cleanup-pending state is visible to `braid status` and the TUI.
- `cli/src/status.rs` `mod tests` -- add a regression test alongside the existing `resolve_alert_state_surfaces_corrupt_latch_as_computation_error` at `:4509`: with no latch, no smartd flag, but a regular file at `alert_cleanup_pending`, `resolve_alert_state` returns an `AlertState` whose causes contain exactly one `ComputationError` whose `detail` names the cleanup-pending condition. Use the literal `//` line-comment preamble convention.
- `docs/decisions/014-alerts.md:47` ("Ack snapshots gating inputs before probing") -- update the paragraph to include `alert-cleanup-pending` as a third entry-snapshot input alongside the alert latch and smartd flag, and to mention the hoisted cleanup-only retry branch that runs before `probe_pool_alerts` so a sentinel-only retry never depends on probe success.
- `docs/decisions/014-alerts.md:103-111` -- add a "Cleanup ordering and retry-on-failure" subsection under "Corrupt latch recovery" naming the three invariants (stop hook attempted first; destructive removals run in `smartd -> latch -> corrupt` order so the forensic sidecar leaves last; `alert-cleanup-pending` sentinel drives the retry). Cross-reference the existing forensic guarantee.
- `manual/commands/ack.md:31-41` -- keep the existing file-removal order (it matches the implementation). Add one sentence to the "What happens under the hood" list: "On a cleanup I/O error, ack preserves retry state so the next `braid ack` resumes cleanup after the I/O fault is fixed." Mechanism-agnostic wording so it covers both halves of the split invariant (post-mark-success path leaves the dedicated marker; pre-mark failure leaves the original entry signals intact).

## Test additions

All new tests use the literal `//` line-comment preamble per `docs/testing.md:11`. Existing fixtures (`cli/src/test_fixtures/ack.rs`) cover most needs: `isolated_paths`, `ack_write_latch`, `ack_mp`, `ack_fs_btrfs`, `ack_fs_not_mounted`, `ack_mounted_probe_runner_with_device_stats`, `AckPanicRunner`. One new fixture lands alongside them: `AckPanicFilesystem`, a zero-field unit struct implementing `Filesystem` whose every method panics with a descriptive message (mirroring the `AckPanicRunner` pattern). The sentinel-only no-baselining test uses it for the retry phase to prove probe-independence -- if a future regression moves the sentinel-only check below `probe_pool_alerts`, the retry's mountinfo read panics and the test fails loudly. `MockRunner`'s keyed-by-debug-string cache (`cli/src/cmd.rs:1282-1373`) handles repeat requests across both invocations.

### Mounted retry (latch-backed)

```rust
// Intent: After cleanup_alert_files_and_beeper fails at remove_alert_latch_corrupt,
//   the alert-cleanup-pending sentinel remains on disk, so the retry re-enters
//   the full ack path, re-invokes the stop hook, and completes cleanup.
// Why it exists: c889f9c made stop_beeper run first but did not change the
//   removal order. The retry on a poisoned corrupt sidecar still hits the
//   no-op gate at ack.rs:67-70 because the latch JSON had already been
//   removed. The sentinel preserves the retry signal without moving the
//   corrupt sidecar's destructive step (which would break ADR 014's
//   forensic guarantee).
// Scenario: monitor latched BtrfsDeviceErrors{devid:1}. A directory sits at
//   alert-latch.json.corrupt. `braid ack` returns CleanupFailed. Operator
//   removes the directory and re-runs `braid ack`.
#[test]
fn cmd_ack_mounted_retry_after_cleanup_failed_completes_recovery() {
    let (_dir, paths) = isolated_paths();
    ack_write_latch(&paths, vec![AlertCause::BtrfsDeviceErrors { devid: 1 }]);
    std::fs::create_dir(paths.alert_latch_corrupt()).unwrap();

    let runner = ack_mounted_probe_runner_with_device_stats();
    let beeper_calls_first = std::cell::Cell::new(0u32);
    let beeper_first = || beeper_calls_first.set(beeper_calls_first.get() + 1);

    let err = cmd_ack_impl(&runner, &ack_fs_btrfs(), &ack_mp(), &paths, &beeper_first)
        .expect_err("first call must fail on the poisoned corrupt sidecar");
    assert!(matches!(err, AckError::CleanupFailed(_)));
    assert_eq!(beeper_calls_first.get(), 1, "stop hook must be invoked on the failing first call");
    assert!(
        paths.alert_cleanup_pending().is_file(),
        "sentinel must remain on disk so retry re-enters cleanup"
    );
    assert!(paths.alert_latch_corrupt().exists(), "poison still wedged");

    std::fs::remove_dir(paths.alert_latch_corrupt()).unwrap();

    let beeper_calls_retry = std::cell::Cell::new(0u32);
    let beeper_retry = || beeper_calls_retry.set(beeper_calls_retry.get() + 1);

    let result = cmd_ack_impl(&runner, &ack_fs_btrfs(), &ack_mp(), &paths, &beeper_retry);
    assert!(result.is_ok(), "retry must succeed after operator clears poison");
    assert_eq!(beeper_calls_retry.get(), 1, "retry must re-invoke the stop hook");
    assert!(!paths.alert_cleanup_pending().exists(), "retry must clear the sentinel");
    assert!(!paths.alert_latch_corrupt().exists());
}
```

### Offline retry (latch-backed)

```rust
// Intent: After the first call's offline ack fails at the corrupt-sidecar
//   removal, the alert-cleanup-pending sentinel is on disk and the latch
//   has been removed. The retry takes the hoisted cleanup-only branch in
//   cmd_ack_impl (before probe_pool_alerts), re-invokes the stop hook, and
//   completes cleanup -- without re-entering ack_offline at all.
// Why it exists: ack_offline's cleanup call at ack.rs:152 shares
//   cleanup_alert_files_and_beeper with the mounted branch, so both
//   first-call paths can leave the sentinel-only state behind. The
//   hoisted branch handles every sentinel-only retry regardless of
//   whether the first call was mounted or offline, so this test pins
//   that the offline first-call leaves a recoverable sentinel state and
//   the hoisted branch resumes it. Without the hoisted branch, the
//   offline retry would land in ack_offline's has_alert == false arm
//   and return PoolNotMounted right after the user followed the
//   CleanupFailed recovery instructions.
// Scenario: pool offline, monitor latched MissingDevice{devid:1}. A
//   directory sits at alert-latch.json.corrupt. Operator runs `braid ack`,
//   gets CleanupFailed, removes the poison, re-runs `braid ack`.
#[test]
fn ack_offline_retry_after_cleanup_failed_completes_recovery() {
    let (_dir, paths) = isolated_paths();
    ack_write_latch(&paths, vec![AlertCause::MissingDevice { devid: 1 }]);
    std::fs::create_dir(paths.alert_latch_corrupt()).unwrap();

    let beeper_calls_first = std::cell::Cell::new(0u32);
    let beeper_first = || beeper_calls_first.set(beeper_calls_first.get() + 1);

    let err = cmd_ack_impl(&AckPanicRunner, &ack_fs_not_mounted(), &ack_mp(), &paths, &beeper_first)
        .expect_err("first call must fail");
    assert!(matches!(err, AckError::CleanupFailed(_)));
    assert_eq!(beeper_calls_first.get(), 1, "stop hook must fire on the failing first call");
    assert!(paths.alert_cleanup_pending().is_file(), "sentinel preserved on cleanup failure");
    assert!(paths.alert_latch_corrupt().exists());

    std::fs::remove_dir(paths.alert_latch_corrupt()).unwrap();

    let beeper_calls_retry = std::cell::Cell::new(0u32);
    let beeper_retry = || beeper_calls_retry.set(beeper_calls_retry.get() + 1);

    let result = cmd_ack_impl(&AckPanicRunner, &ack_fs_not_mounted(), &ack_mp(), &paths, &beeper_retry);
    assert!(result.is_ok(), "offline retry must succeed after operator clears poison");
    assert_eq!(beeper_calls_retry.get(), 1, "retry must re-invoke the stop hook");
    assert!(!paths.alert_cleanup_pending().exists(), "retry must clear the sentinel");
    assert!(!paths.alert_latch_corrupt().exists());
    // missing_acked persists (set on the first call, re-applied on the retry
    // -- idempotent insert-or-update via the existing ack_offline path).
    let acked = load_acked_stats(&paths);
    assert!(acked.0.get("1").unwrap().missing_acked);
}
```

### Mounted retry (smartd-only, no latch)

```rust
// Intent: When the entry alert signal is a smartd flag and no latch JSON
//   exists, a cleanup failure at remove_alert_latch_corrupt leaves the
//   sentinel on disk, so the retry re-enters cleanup even though smartd
//   was already removed during the first attempt.
// Why it exists: The latch-backed retry tests still pass if a future
//   refactor narrowed the sentinel to "latch present" cases. This test
//   pins that the sentinel covers the smartd-only path too -- the path
//   where neither a latch nor an active smartd flag survives the first
//   call's cleanup, but the sentinel still drives the retry.
// Scenario: smartd hook fired (smartd-alert is a real file) but monitor
//   has not run yet, so there is no latch JSON. A directory sits at
//   alert-latch.json.corrupt. `braid ack` saves a fresh baseline,
//   removes the smartd flag, the stop hook fires, cleanup fails at the
//   corrupt sidecar, and the sentinel remains. Operator removes the
//   directory and re-runs.
#[test]
fn cmd_ack_mounted_smartd_only_retry_after_cleanup_failed_completes_recovery() {
    let (_dir, paths) = isolated_paths();
    std::fs::write(paths.smartd_alert(), b"").unwrap();
    std::fs::create_dir(paths.alert_latch_corrupt()).unwrap();

    let runner = ack_mounted_probe_runner_with_device_stats();
    let beeper_calls_first = std::cell::Cell::new(0u32);
    let beeper_first = || beeper_calls_first.set(beeper_calls_first.get() + 1);

    let err = cmd_ack_impl(&runner, &ack_fs_btrfs(), &ack_mp(), &paths, &beeper_first)
        .expect_err("first call must fail on the poisoned corrupt sidecar");
    assert!(matches!(err, AckError::CleanupFailed(_)));
    assert_eq!(beeper_calls_first.get(), 1, "stop hook must be invoked on the failing first call");
    assert!(
        !paths.smartd_alert().exists(),
        "smartd flag was removed before the corrupt-sidecar step failed"
    );
    assert!(paths.alert_cleanup_pending().is_file(), "sentinel preserved on cleanup failure");
    assert!(paths.alert_latch_corrupt().exists(), "poison still wedged");

    std::fs::remove_dir(paths.alert_latch_corrupt()).unwrap();

    let beeper_calls_retry = std::cell::Cell::new(0u32);
    let beeper_retry = || beeper_calls_retry.set(beeper_calls_retry.get() + 1);

    let result = cmd_ack_impl(&runner, &ack_fs_btrfs(), &ack_mp(), &paths, &beeper_retry);
    assert!(result.is_ok(), "retry must succeed after operator clears poison");
    assert_eq!(beeper_calls_retry.get(), 1, "retry must re-invoke the stop hook");
    assert!(!paths.alert_cleanup_pending().exists(), "retry must clear the sentinel");
    assert!(!paths.alert_latch_corrupt().exists());
}
```

### Sentinel-only retry is cleanup-only, not a fresh ack

```rust
// Intent: When the retry's only entry signal is the cleanup-pending sentinel,
//   cmd_ack_impl takes the hoisted sentinel-only branch that runs BEFORE
//   probe_pool_alerts. The retry issues zero runner requests (no probe, no
//   BtrfsDeviceStatsJson) and does not rewrite acked-stats.json. The retry
//   is finishing a previous ack, not starting a new one, so the saved
//   baseline must stay byte-identical.
// Why it exists: A naive sentinel-aware gate placed after probe_pool_alerts
//   would re-wedge cleanup on any probe failure (NotBtrfs, mountinfo I/O,
//   etc.) on the retry path, and would also fall through to the full
//   mounted ack pipeline -- running BtrfsDeviceStatsJson, recomputing
//   snapshot_current, and writing a fresh acked-stats.json baseline. New
//   counter increments that arrived after the failed first call would be
//   silently folded into the new baseline and never alert. The hoisted
//   branch is probe-independent and baseline-preserving; this test pins
//   both properties as one regression class.
// Scenario: monitor latched BtrfsDeviceErrors{devid:1}, mounted btrfs, healthy
//   first-call device stats. A directory sits at alert-latch.json.corrupt.
//   First call's cleanup fails at the corrupt sidecar step; baseline is saved
//   and the sentinel is left on disk. Operator removes the directory. Retry
//   must complete cleanup without re-querying btrfs.
#[test]
fn cmd_ack_mounted_sentinel_only_retry_does_not_query_btrfs_or_rewrite_baseline() {
    let (_dir, paths) = isolated_paths();
    ack_write_latch(&paths, vec![AlertCause::BtrfsDeviceErrors { devid: 1 }]);
    std::fs::create_dir(paths.alert_latch_corrupt()).unwrap();

    let runner = ack_mounted_probe_runner_with_device_stats();
    let beeper_calls_first = std::cell::Cell::new(0u32);
    let beeper_first = || beeper_calls_first.set(beeper_calls_first.get() + 1);

    let err = cmd_ack_impl(&runner, &ack_fs_btrfs(), &ack_mp(), &paths, &beeper_first)
        .expect_err("first call must fail on the poisoned corrupt sidecar");
    assert!(matches!(err, AckError::CleanupFailed(_)));
    let baseline_after_first = std::fs::read(paths.acked_stats_json()).unwrap();
    let requests_after_first = runner.requests().len();

    std::fs::remove_dir(paths.alert_latch_corrupt()).unwrap();

    let beeper_calls_retry = std::cell::Cell::new(0u32);
    let beeper_retry = || beeper_calls_retry.set(beeper_calls_retry.get() + 1);
    // Use a filesystem fixture that panics on any access. If the retry
    // touches probe_pool_alerts (which reads mountinfo) the test fails
    // loudly with the panic, not silently because a healthy fixture let
    // the probe succeed. Pairs with the AckPanicRunner pattern.
    let result = cmd_ack_impl(&runner, &AckPanicFilesystem, &ack_mp(), &paths, &beeper_retry);
    assert!(result.is_ok(), "retry must succeed without probing");

    // The retry must take the hoisted sentinel-only branch, which runs
    // before probe_pool_alerts. No new runner requests of any kind --
    // no BtrfsFilesystemShow / CryptsetupStatus / BtrfsDeviceStatsJson.
    let retry_requests: Vec<_> = runner.requests().into_iter().skip(requests_after_first).collect();
    assert!(
        retry_requests.is_empty(),
        "sentinel-only retry must issue zero runner requests -- it must not probe or query btrfs stats; got {retry_requests:?}"
    );
    let baseline_after_retry = std::fs::read(paths.acked_stats_json()).unwrap();
    assert_eq!(
        baseline_after_first, baseline_after_retry,
        "sentinel-only retry must not rewrite acked-stats.json"
    );
    assert!(!paths.alert_cleanup_pending().exists(), "retry must clear the sentinel");
    assert!(!paths.alert_latch_corrupt().exists());
}
```

The retry's `&AckPanicFilesystem` is a new fixture added to `cli/src/test_fixtures/ack.rs` alongside `AckPanicRunner` -- a zero-field unit struct implementing `Filesystem` where every method panics with a descriptive message ("sentinel-only retry must not touch the filesystem; got `<op>(<path>)`"). The Intent comment on this fixture mirrors `AckPanicRunner`'s: it guards the no-runner-work / no-probe-work boundary by failing loudly if a future regression re-introduces filesystem access on the sentinel-only retry path.

### Marker-creation failure: entry signals drive retry

```rust
// Intent: When `mark_alert_cleanup_pending` itself fails (the sentinel path
//   is a directory, permission drift, etc.), cleanup short-circuits before
//   any destructive removal runs. CleanupFailed is returned; every entry
//   alert signal (latch JSON, smartd flag, corrupt sidecar) is byte-
//   identical to entry, and the retry observes the original entry snapshot
//   to drive re-entry into cleanup.
// Why it exists: The cleanup-pending sentinel is the primary retry signal,
//   but the sentinel itself is a file ack writes -- so it has the same
//   class of poison failures as every other alert-state path. The retry-
//   converges invariant splits into two cases: pre-mark failure preserves
//   entry signals; post-mark failure preserves the sentinel. A regression
//   that ordered any destructive removal before `mark_alert_cleanup_pending`
//   would pass every other test while quietly destroying alert state before
//   the marker had a chance to record cleanup-pending.
// Scenario: a directory sits at alert-cleanup-pending (operator mistake,
//   permission drift, leftover bug). Latch carries BtrfsDeviceErrors{
//   devid:1}, mounted btrfs, healthy device stats. cmd_ack must save the
//   baseline (this happens before cleanup), invoke the stop hook, fail at
//   the marker step, and leave every alert file unchanged. After the
//   operator removes the poison directory, the retry completes cleanly.
#[test]
fn cmd_ack_mounted_retry_after_poisoned_sentinel_completes_recovery() {
    let (_dir, paths) = isolated_paths();
    ack_write_latch(&paths, vec![AlertCause::BtrfsDeviceErrors { devid: 1 }]);
    std::fs::create_dir(paths.alert_cleanup_pending()).unwrap();
    let original_latch_bytes = std::fs::read(paths.alert_latch_json()).unwrap();

    let runner = ack_mounted_probe_runner_with_device_stats();
    let beeper_calls_first = std::cell::Cell::new(0u32);
    let beeper_first = || beeper_calls_first.set(beeper_calls_first.get() + 1);

    let err = cmd_ack_impl(&runner, &ack_fs_btrfs(), &ack_mp(), &paths, &beeper_first)
        .expect_err("marker creation must fail on the poisoned sentinel path");
    assert!(matches!(err, AckError::CleanupFailed(_)));
    assert_eq!(beeper_calls_first.get(), 1, "stop hook must fire before marker creation");

    // Entry signals are byte-identical: no destructive removal has run.
    assert_eq!(
        std::fs::read(paths.alert_latch_json()).unwrap(),
        original_latch_bytes,
        "latch JSON must be preserved -- retry drives re-execution from the entry snapshot"
    );
    assert!(paths.alert_cleanup_pending().exists(), "poison sentinel directory still wedged");
    // is_file() rejects the directory, so cleanup_pending stays false and
    // the retry's gate redirects via the latch, not the sentinel.
    assert!(
        !alert::alert_cleanup_pending(&paths),
        "directory-form sentinel must not be treated as cleanup-pending"
    );
    assert!(paths.acked_stats_json().exists(), "save_acked_stats ran before cleanup");

    std::fs::remove_dir(paths.alert_cleanup_pending()).unwrap();

    let beeper_calls_retry = std::cell::Cell::new(0u32);
    let beeper_retry = || beeper_calls_retry.set(beeper_calls_retry.get() + 1);
    let result = cmd_ack_impl(&runner, &ack_fs_btrfs(), &ack_mp(), &paths, &beeper_retry);
    assert!(result.is_ok(), "retry must succeed after operator clears the sentinel poison");
    assert_eq!(beeper_calls_retry.get(), 1, "retry must re-invoke the stop hook");
    assert!(!paths.alert_cleanup_pending().exists());
    assert!(!paths.alert_latch_json().exists());
}
```

### Forensic preservation through cleanup failure

```rust
// Intent: When ack cleanup fails before reaching remove_alert_latch_corrupt,
//   the corrupt sidecar's bytes are unchanged. Pins ADR 014's forensic
//   guarantee at the unit level for the cleanup-failure path: bad bytes
//   are preserved across CleanupFailed, not just across successful ack.
// Why it exists: The cleanup-pending sentinel design exists specifically to
//   keep alert-latch.json.corrupt as the last destructive step so its bytes
//   survive partial cleanup. tests/cli/braid-monitor.py:208-242 covers
//   sidecar preservation across monitor cycles up to a successful braid
//   ack; this test pins the missing path -- preservation across an ack
//   that fails partway. Without it, a future cleanup reorder that moved
//   remove_alert_latch_corrupt earlier would pass every other ack test
//   while silently destroying forensic evidence whenever a later removal
//   failed.
// Scenario: a prior monitor cycle quarantined a corrupt latch, leaving
//   forensic bytes at alert-latch.json.corrupt. The current cycle latched
//   SmartdAlert, but the smartd flag path is a poison directory. cmd_ack
//   must save the baseline, invoke the stop hook, mark cleanup-pending,
//   fail at remove_smartd_alert_flag, and leave the corrupt sidecar's
//   bytes untouched.
#[test]
fn cmd_ack_preserves_corrupt_sidecar_bytes_through_cleanup_failure() {
    let (_dir, paths) = isolated_paths();
    let forensic_bytes: &[u8] = b"first corruption forensic data";
    std::fs::write(paths.alert_latch_corrupt(), forensic_bytes).unwrap();
    ack_write_latch(&paths, vec![AlertCause::SmartdAlert]);
    std::fs::create_dir(paths.smartd_alert()).unwrap();

    let runner = ack_mounted_probe_runner_with_device_stats();
    let beeper_calls = std::cell::Cell::new(0u32);
    let beeper = || beeper_calls.set(beeper_calls.get() + 1);

    let err = cmd_ack_impl(&runner, &ack_fs_btrfs(), &ack_mp(), &paths, &beeper)
        .expect_err("smartd cleanup failure must propagate");
    assert!(matches!(err, AckError::CleanupFailed(_)));
    assert_eq!(beeper_calls.get(), 1);

    let preserved = std::fs::read(paths.alert_latch_corrupt()).unwrap();
    assert_eq!(
        preserved, forensic_bytes,
        "corrupt sidecar bytes must survive cleanup failure -- ADR 014 forensic guarantee"
    );
    assert!(
        paths.alert_cleanup_pending().is_file(),
        "sentinel must be on disk -- mark_* succeeded before the smartd-removal failure"
    );
}
```

### Adjustments to the existing partial-state tests

The mounted partial-state test at `ack.rs:663-707` and the offline equivalent at `ack.rs:976-...` already pin `acked_stats_json` exists / `alert_latch_json` removed / `alert_latch_corrupt` exists / `beeper_calls == 1`. Add one new witness to each: `paths.alert_cleanup_pending().is_file()`. The destructive order is unchanged so no existing witness flips. Update the Intent/Why-it-exists preambles to name the sentinel as the recovery signal.

The `cmd_ack_stops_beeper_before_mounted_smartd_flag_cleanup_error` test at `ack.rs:709-756` (the early-removal failure -- smartd flag is a poison directory) similarly grows one assertion: the sentinel is on disk after `CleanupFailed`. Failure occurs at the smartd step, so `mark_*` succeeded and the sentinel was written. No other assertion changes.

## Doc updates

- `cli/src/ack.rs:159-182` (`cleanup_alert_files_and_beeper` rustdoc): keep the existing best-effort-stop paragraph (`:168-172`). Add a paragraph describing the sentinel contract: `mark_alert_cleanup_pending` runs after `stop_beeper` and before any destructive removal; destructive removals run in `smartd -> latch -> corrupt` order so `alert-latch.json.corrupt` is the last destructive step (ADR 014 forensic guarantee); `clear_alert_cleanup_pending` runs only after every `remove_*` succeeds. State the split retry-converges invariant: if `mark_*` itself fails, no destructive removal has run and the original entry alert signals are byte-identical to entry; if `mark_*` succeeded, any subsequent `?` short-circuit leaves the sentinel on disk. The retry has a signal in either case.
- `cli/src/ack.rs:247-257` (`AckError::CleanupFailed` rustdoc): expand to state that the stop hook has been attempted (best-effort) and the retry has a signal to drive re-execution. Document the split explicitly: if `mark_alert_cleanup_pending` itself failed, no destructive removal has run and the original entry signals (latch JSON, smartd flag, corrupt sidecar) are byte-identical to entry, so the retry's regular ack path observes them and re-enters cleanup; if `mark_*` succeeded and a later step failed, the `alert-cleanup-pending` sentinel is on disk and the hoisted cleanup-only branch in `cmd_ack_impl` (which runs before `probe_pool_alerts`) re-enters cleanup directly, with no probe / runner / `BtrfsDeviceStatsJson` / `save_acked_stats` work. Re-running `braid ack` after fixing the I/O fault is NotFound-tolerant on every removal.
- `docs/decisions/014-alerts.md` -- two edits per AGENTS.md (any behavior/invariant change must update architecture docs):
  - `:47` ("Ack snapshots gating inputs before probing"): expand the paragraph to include `alert-cleanup-pending` as a third entry-snapshot input alongside the alert latch and the smartd flag, and note that a sentinel-only retry takes a dedicated cleanup-only branch *before* `probe_pool_alerts` so it never depends on probe success.
  - After `:111`: add a "Cleanup ordering and retry-on-failure" subsection under "Corrupt latch recovery" listing the three invariants -- stop hook attempted before any fallible cleanup; destructive removals in `smartd -> latch -> corrupt` order so `alert-latch.json.corrupt` is removed last and the forensic guarantee at `:107,111` is preserved across cleanup failures; an explicit `alert-cleanup-pending` sentinel that ack writes after the stop hook and before the first destructive step, and clears only after the last destructive step. Document the split retry-converges contract: if marker creation itself fails, no destructive removal has run and the retry is driven by the unchanged entry signals; if marker creation succeeded and a later step failed, the sentinel is on disk. The hoisted cleanup-only branch in `cmd_ack` consults the sentinel before probing so a sentinel-only retry never depends on probe success or re-baselines `acked-stats.json`. Either path makes `CleanupFailed` recovery genuinely idempotent.
- `manual/commands/ack.md:31-41`: keep the existing 4 / 5 / 6 file-removal order unchanged. Add one sentence to the "What happens under the hood" list with user-facing wording that abstracts over the internal mechanism: "On a cleanup I/O error, ack preserves retry state so the next `braid ack` resumes cleanup after the I/O fault is fixed." This is true for both cases of the split invariant (the dedicated marker file when `mark_*` succeeded, the unchanged entry signals when `mark_*` itself failed) without overcommitting to a single mechanism that fits only one path.

## Verification

1. `just test-rust` -- runs every required test added in this plan: the three ack retry tests (mounted latch-backed, offline latch-backed, mounted smartd-only), the sentinel-only no-baselining test (`cmd_ack_mounted_sentinel_only_retry_does_not_query_btrfs_or_rewrite_baseline`), the marker-creation-failure retry test (`cmd_ack_mounted_retry_after_poisoned_sentinel_completes_recovery`), the forensic-preservation unit test (`cmd_ack_preserves_corrupt_sidecar_bytes_through_cleanup_failure`), the new `resolve_alert_state` sentinel test, the updated existing partial-state and stop-beeper-before tests, the four new `alert.rs` helper unit tests (including the `#[cfg(unix)]` permission-drift test that pins the `is_file()` short-circuit on a read-only sentinel), and every other ack/alert/status test. All must pass. The sentinel-only test pins probe-independence (uses `AckPanicFilesystem` so any retry-side mountinfo read panics) plus the no-baselining contract; the marker-failure test pins the pre-mark half of the retry-converges invariant; the forensic test pins the ADR 014 guarantee for the cleanup-failure path that the existing VM test only covers for successful ack; the status test pins that post-mark cleanup-pending state is visible in `braid status` (and through it the TUI); the permission-drift helper test pins that an existing read-only sentinel does not re-wedge subsequent acks at `mark_*`.
2. `cargo check -p braid-cli` -- no compile errors from the new helpers, the `ack_offline` signature change, the new accessor, the `resolve_alert_state` change, or the rustdoc edits.
3. `just test-vm braid-monitor` -- the existing "Repeated corrupt latch preserves first sidecar" VM subtest at `tests/cli/braid-monitor.py:208-242` must remain green; the new ack behavior does not change monitor's quarantine semantics or the post-`braid ack` clean state.
4. Manually inspect the diff to confirm: `cli/src/ack.rs` destructive-removal order is still `smartd -> latch -> corrupt` (only `mark_*` and `clear_*` are added around it); `stop_beeper()` is still the first call; the hoisted sentinel-only branch sits *before* `probe_pool_alerts` and calls `cleanup_alert_files_and_beeper(paths, stop_beeper, false)` directly with no probe / runner / `BtrfsDeviceStatsJson` / `save_acked_stats` work; the sentinel accessor exists in `state_paths.rs`; the three new helpers exist in `alert.rs` with the `is_file()` short-circuit at the head of `mark_*` (verified by the read-only `#[cfg(unix)]` test); the new `AckPanicFilesystem` fixture exists in `cli/src/test_fixtures/ack.rs`; `resolve_alert_state` in `cli/src/status.rs` consults `alert_cleanup_pending` and surfaces a `ComputationError` cause; the seven new ack/status tests plus the four new alert-helper tests sit adjacent to their sibling tests; `docs/decisions/014-alerts.md` has *both* edits (the `:47` snapshot paragraph expansion and the new "Cleanup ordering and retry-on-failure" subsection after `:111`); `manual/commands/ack.md` has the user-facing "preserves retry state" sentence.

No other VM test is needed -- this is unit-level orchestration in `ack.rs` plus a small `alert.rs` helper addition. Parser canaries (`just test-parsers`) are unaffected.
