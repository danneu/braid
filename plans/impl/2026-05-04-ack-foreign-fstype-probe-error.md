# Fix: `cmd_ack` misclassifies `ProbeError::NotBtrfs` as "pool offline"

## Context

`cli/src/ack.rs:34-36` routes `ProbeError::NotBtrfs` (the configured mount point is held by a non-btrfs filesystem, e.g. ext4 left over from an OS reinstall) into `ack_offline`. The result:

- If any alert is latched: `ack_offline` clears the latch, the smartd flag, mutates `acked-stats.json` for `MissingDevice` causes, calls `stop_beeper()`, and prints `acknowledged current alerts`. The user never sees the real condition (`/mnt/storage is mounted but fstype is ext4, not btrfs`), the latch is silently destroyed, and durable acked-state may be mutated against a foreign-fstype state.
- If no alert is latched: `cmd_ack` returns `AckError::PoolNotMounted` (`pool is not mounted -- nothing to acknowledge`), which is also untrue -- the pool *is* mounted, just not as btrfs.

This is reachable only by operator error (manual mount of a non-btrfs FS at the configured mount point). Severity is **Low**, but the failure mode is silent state loss plus a misleading message.

`probe_pool`'s `Display` already produces the exact message we want: `"{mount_point} is mounted but fstype is {fstype}, not btrfs"` (`probe.rs:65-66`). The fix is to stop swallowing the variant. This matches the precedent set in `add.rs:896-901`.

## Important: existing tests use `Ext4Fs` as a proxy for "pool offline"

The current test suite (`ack.rs:175-722`) has seven `ack_offline_*` tests that pass `&Ext4Fs` to `cmd_ack` to reach the offline path -- they relied on the buggy `NotBtrfs ⇒ ack_offline` routing. Their actual scenario is "pool not mounted" (operator locked the pool), not "operator mounted ext4". After the fix, those calls would fail with `AckError::Probe(NotBtrfs)`. They must be migrated to a `NotMountedFs` mock that exercises the genuine `pool.mounted == false` branch (`ack.rs:40-42`).

Affected tests (all in `ack.rs`, all currently call `cmd_ack(&PanicRunner, &Ext4Fs, ...)`):

- `ack_offline_with_missing_device_cause_marks_missing_acked` (488)
- `ack_offline_refuses_when_btrfs_errors_mixed_with_missing` (517)
- `ack_offline_preserves_existing_device_stats_baseline` (558)
- `ack_offline_corrupt_latch_still_clears_files` (602)
- `ack_offline_corrupt_acked_stats_propagates_io_error_when_missing_cause` (633)
- `ack_offline_smartd_only_latch_does_not_load_acked_stats` (670)
- `ack_offline_computation_error_only_latch_does_not_load_acked_stats` (704)

## Fix

### 1. `cli/src/ack.rs` -- drop the `NotBtrfs` special case

In `cmd_ack`, delete the explicit `NotBtrfs` arm (lines 34-36) so it falls through to the generic `Err(e)` propagation:

```rust
// Before (lines 32-38):
let pool = match probe_pool(runner, fs, mount_point) {
    Ok(p) => p,
    Err(ProbeError::NotBtrfs { .. }) => {
        return ack_offline(latch_state, latch_corrupt, paths);
    }
    Err(e) => return Err(AckError::Probe(e)),
};

// After:
let pool = match probe_pool(runner, fs, mount_point) {
    Ok(p) => p,
    Err(e) => return Err(AckError::Probe(e)),
};
```

The `if !pool.mounted { return ack_offline(...) }` line (40-42) is unchanged. All of `ack_offline`'s newer responsibilities (`AlertState` carrying, `BtrfsDeviceErrors` refusal at 100-107, fallible `load_acked_stats_fallible` mutation at 127-133) remain intact.

Also update the unreadable-latch warning to avoid claiming ack will proceed:

```rust
eprintln!("warning: alert latch unreadable -- treating as active for ack gating: {e}");
```

This wording stays true for both outcomes: mounted or genuinely unmounted ack may still clean up the corrupt latch, while `NotBtrfs` now returns a probe error and preserves the unreadable latch bytes.

### 2. `cli/src/ack.rs` -- add a `cmd_ack_impl` hook for tests that need to observe the beeper, but keep the cfg-split

Today `stop_beeper` is split (`cfg(not(test))` real systemctl, `cfg(test)` no-op, lines 143-157). That keeps every test that calls public `cmd_ack` host-safe -- it must stay. What's missing is a way for *specific* tests to assert `stop_beeper` was (or was not) invoked, so a regression that drops the call from `ack_offline` would slip through.

Add a `cmd_ack_impl` taking a `&dyn Fn()` beeper hook for that subset of tests, and **keep both cfg variants of `stop_beeper`**. Most tests keep calling public `cmd_ack` and remain insulated from real systemctl. Only the tests that need to assert beeper behavior switch to `cmd_ack_impl(..., &recording_closure)`.

```rust
pub fn cmd_ack<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    mount_point: &MountPoint,
    paths: &StatePaths,
) -> Result<(), AckError> {
    cmd_ack_impl(runner, fs, mount_point, paths, &stop_beeper)
}

fn cmd_ack_impl<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    mount_point: &MountPoint,
    paths: &StatePaths,
    stop_beeper: &dyn Fn(),
) -> Result<(), AckError> {
    /* current cmd_ack body, with the NotBtrfs arm removed and the two
       stop_beeper() free-fn calls replaced by stop_beeper() (the param).
       The ack_offline call site forwards the closure. */
}

fn ack_offline(
    latch_state: Option<AlertState>,
    latch_corrupt: bool,
    paths: &StatePaths,
    stop_beeper: &dyn Fn(),
) -> Result<(), AckError> {
    /* current body; the line-138 stop_beeper() free-fn call becomes the param. */
}

#[cfg(not(test))]
fn stop_beeper() {
    let result = std::process::Command::new("systemctl")
        .args(["stop", "braid-alert.service"])
        .output();
    match result {
        Err(e) => {
            eprintln!("warning: could not stop braid-alert.service: {e}");
        }
        Ok(output) => {
            if let Some(msg) = format_systemctl_stop_failure(&output) {
                eprintln!("{msg}");
            }
        }
    }
}

fn format_systemctl_stop_failure(output: &std::process::Output) -> Option<String> {
    /* keep current helper body unchanged */
}

// Unit tests that exercise public cmd_ack must not shell out to real
// systemctl on the host. cmd_ack_impl callers in tests that need to
// observe beeper invocation pass their own closure instead.
#[cfg(test)]
fn stop_beeper() {}
```

`main.rs:664` is unaffected (still calls public `cmd_ack`). Trait bound is `&dyn Fn()` (not `FnOnce`) because the type system can't statically prove only one branch fires per call. Tests that don't care about beeper invocation (the six migrated `ack_offline_*` tests, the existing mounted-ack tests at 411/449) keep calling public `cmd_ack` and pick up the cfg(test) no-op transparently.

Keep the current `format_systemctl_stop_failure` helper and its two unit tests unchanged. The hook refactor should only replace the call sites that invoke `stop_beeper()` from `cmd_ack_impl` / `ack_offline`; it must not regress the existing warning for non-zero `systemctl stop braid-alert.service` exits.

### 3. `cli/src/ack.rs` -- add `NotMountedFs` and migrate the seven `ack_offline_*` tests

Add a `NotMountedFs` mock alongside `Ext4Fs` and `BtrfsFs` (around `ack.rs:225`). It returns mountinfo with no entry for `/mnt/storage`, which makes `fstype_at_mount_via_fs` return `None` and `probe_pool` return `Ok(PoolState { mounted: false, .. })` -- the genuine offline branch.

```rust
struct NotMountedFs;

impl Filesystem for NotMountedFs {
    fn exists(&self, _path: &str) -> bool { false }
    fn is_block_device(&self, _path: &str) -> bool { false }
    fn read_to_string(&self, path: &str) -> Result<String, std::io::Error> {
        assert_eq!(path, "/proc/self/mountinfo");
        // mountinfo body without any /mnt/storage entry -> fstype_at_mount returns None
        Ok(String::new())
    }
    fn list_dir(&self, _path: &str) -> Result<Vec<String>, std::io::Error> { Ok(vec![]) }
}
```

Then in each of the seven listed tests, replace `&Ext4Fs` with `&NotMountedFs`. The `&PanicRunner` argument stays -- both paths return before any runner call. No assertion changes; the offline behavior under inspection is identical to what the tests intended. Each test's block comment can stay as-is (the scenarios genuinely describe offline ack), but consider a one-line addition noting the mock now reflects "pool genuinely not mounted" rather than "ext4 mounted at the path".

### 4. `cli/src/ack.rs` -- add public regression tests for the genuine `NotBtrfs` case

Three new tests using public `cmd_ack` and `&Ext4Fs` (which now means what its name says -- mount target held by ext4). These tests must use the public entrypoint because `main.rs:664` calls `cmd_ack`; the regression guard needs to fail if `cmd_ack` itself remains miswired or keeps the old `NotBtrfs -> ack_offline` behavior.

```rust
/*
 * Intent: cmd_ack must surface ProbeError::NotBtrfs to the caller and
 *   leave latched alert state intact when an alert is already on disk.
 * Why it exists: Prior behavior silently deleted the latch + smartd flag,
 *   mutated acked-stats for any latched MissingDevice cause, and printed
 *   "acknowledged current alerts" for a state that is not actually offline.
 *   Pins the regression guard for the with-alerts case (the more
 *   dangerous branch, since it mutates persistent state).
 * Scenario: operator left an ext4 partition mounted at /mnt/storage. A
 *   pre-existing alert latch (with a MissingDevice cause that would
 *   otherwise trigger acked-stats mutation in ack_offline) and smartd flag
 *   are on disk. Running `braid ack` must error out without touching
 *   alert-latch.json, smartd-alert, or acked-stats.json.
 */
#[test]
fn cmd_ack_with_foreign_fstype_and_alerts_returns_probe_error_and_preserves_state() {
    let (_dir, paths) = fresh_paths();
    write_latch(&paths, vec![AlertCause::MissingDevice { devid: 2 }]);
    let original_latch_bytes = std::fs::read(paths.alert_latch_json()).unwrap();
    std::fs::write(paths.smartd_alert(), b"").unwrap();

    let err = cmd_ack(&PanicRunner, &Ext4Fs, &mp(), &paths)
        .expect_err("must refuse -- mount is not btrfs");

    match &err {
        AckError::Probe(ProbeError::NotBtrfs { fstype, .. }) => {
            assert_eq!(fstype, "ext4");
        }
        other => panic!("expected AckError::Probe(NotBtrfs), got: {other:?}"),
    }
    let msg = err.to_string();
    assert!(
        msg.contains("not btrfs") && msg.contains("ext4"),
        "user-visible message must name fstype, got: {msg}"
    );

    assert_eq!(
        std::fs::read(paths.alert_latch_json()).unwrap(),
        original_latch_bytes,
        "latch bytes must be preserved"
    );
    assert!(paths.smartd_alert().exists(), "smartd flag must be preserved");
    assert!(
        !paths.acked_stats_json().exists(),
        "acked-stats must not be created from a NotBtrfs path"
    );
}

/*
 * Intent: With no pre-existing alerts, NotBtrfs must surface the real
 *   condition rather than `AckError::PoolNotMounted`.
 * Why it exists: Prior behavior returned "pool is not mounted -- nothing
 *   to acknowledge", a lie. Pins the no-alert branch so it cannot regress
 *   independently of the with-alerts branch.
 * Scenario: clean state directory (no latch, no smartd flag), but the
 *   mount target holds ext4. `braid ack` must report the fstype, not
 *   claim the pool is unmounted, and must not create any alert files.
 */
#[test]
fn cmd_ack_with_foreign_fstype_and_no_alerts_returns_probe_error() {
    let (_dir, paths) = fresh_paths();

    let err = cmd_ack(&PanicRunner, &Ext4Fs, &mp(), &paths)
        .expect_err("must refuse -- mount is not btrfs");

    match &err {
        AckError::Probe(ProbeError::NotBtrfs { fstype, .. }) => {
            assert_eq!(fstype, "ext4");
        }
        other => panic!("expected AckError::Probe(NotBtrfs), got: {other:?}"),
    }
    let msg = err.to_string();
    assert!(
        msg.contains("not btrfs") && msg.contains("ext4"),
        "user-visible message must name fstype, got: {msg}"
    );

    assert!(!paths.alert_latch_json().exists(), "no latch should appear");
    assert!(!paths.smartd_alert().exists(), "no smartd flag should appear");
    assert!(!paths.acked_stats_json().exists(), "no acked-stats should appear");
}

/*
 * Intent: A corrupt alert-latch.json plus a foreign fstype still surfaces
 *   ProbeError::NotBtrfs and preserves the unreadable latch bytes.
 * Why it exists: cmd_ack reads the latch before probing the pool. The corrupt
 *   latch must count as active for gating, but a non-btrfs mount target is not
 *   a genuine offline pool, so ack must not clean up the corrupt latch on this
 *   path.
 * Scenario: alert-latch.json contains garbage bytes, and an ext4 filesystem is
 *   mounted at /mnt/storage. `braid ack` must report the fstype mismatch and
 *   leave the corrupt file available for later ack after the mount is fixed.
 */
#[test]
fn cmd_ack_with_foreign_fstype_and_corrupt_latch_preserves_latch_bytes() {
    let (_dir, paths) = fresh_paths();
    std::fs::write(paths.alert_latch_json(), b"not json").unwrap();
    let original_latch_bytes = std::fs::read(paths.alert_latch_json()).unwrap();

    let err = cmd_ack(&PanicRunner, &Ext4Fs, &mp(), &paths)
        .expect_err("must refuse -- mount is not btrfs");

    assert!(
        matches!(err, AckError::Probe(ProbeError::NotBtrfs { .. })),
        "expected AckError::Probe(NotBtrfs), got: {err:?}"
    );
    assert_eq!(
        std::fs::read(paths.alert_latch_json()).unwrap(),
        original_latch_bytes,
        "corrupt latch bytes must be preserved on NotBtrfs"
    );
    assert!(!paths.alert_latch_corrupt().exists(), "NotBtrfs must not quarantine or clean up the latch");
    assert!(!paths.acked_stats_json().exists(), "no acked-stats should appear");
}
```

Then add one narrow `cmd_ack_impl`-only test for the beeper non-invocation assertion. This test is not the main behavior guard; it exists only because public `cmd_ack` deliberately uses the `cfg(test)` no-op `stop_beeper` in unit tests.

```rust
/*
 * Intent: The NotBtrfs error path must not invoke the beeper hook.
 * Why it exists: Prior behavior routed NotBtrfs through ack_offline, whose
 *   success path stops the beeper. The public cmd_ack tests above pin the
 *   user-visible error and state preservation; this hook-only test pins the
 *   side-effect boundary.
 * Scenario: mount target holds ext4 and a latch exists. The implementation
 *   returns Probe(NotBtrfs) before reaching any ack_offline cleanup.
 */
#[test]
fn cmd_ack_impl_with_foreign_fstype_does_not_invoke_beeper() {
    let (_dir, paths) = fresh_paths();
    write_latch(&paths, vec![AlertCause::MissingDevice { devid: 2 }]);
    let beeper_calls = std::cell::Cell::new(0u32);
    let beeper = || beeper_calls.set(beeper_calls.get() + 1);

    let err = cmd_ack_impl(&PanicRunner, &Ext4Fs, &mp(), &paths, &beeper)
        .expect_err("must refuse -- mount is not btrfs");

    assert!(
        matches!(err, AckError::Probe(ProbeError::NotBtrfs { .. })),
        "expected AckError::Probe(NotBtrfs), got: {err:?}"
    );
    assert_eq!(beeper_calls.get(), 0, "stop_beeper must not be called on NotBtrfs");
}
```

### 5. `cli/src/ack.rs` -- pin the offline-ack beeper invocation

Update one existing offline-ack test (recommended: `ack_offline_with_missing_device_cause_marks_missing_acked` -- the canonical happy-path offline-ack regression gate) to switch from public `cmd_ack` to `cmd_ack_impl(..., &recording_beeper)`, with a `Cell<u32>` counter, and assert `stop_beeper` fired exactly once. This pins the production wiring that monitor-lifecycle does not cover (it tests only mounted ack):

```rust
let beeper_calls = std::cell::Cell::new(0u32);
let beeper = || beeper_calls.set(beeper_calls.get() + 1);
cmd_ack_impl(&PanicRunner, &NotMountedFs, &mp(), &paths, &beeper).unwrap();
assert_eq!(beeper_calls.get(), 1, "stop_beeper must fire once on offline-ack success");
// ... existing assertions on missing_acked and latch removal ...
```

The other six migrated tests stay on public `cmd_ack` -- a single offline-ack-fires-beeper assertion is enough to pin the call.

### 6. Documentation -- distinguish genuine offline ack from foreign-fstype probe errors

Update the user-facing ack manual and ADR 014 to make the new boundary explicit:

- `manual/commands/ack.md` -- in the "What happens under the hood" / safety-check area, add a short note that "pool offline" means no mount at the configured mount point. If the configured mount point is occupied by a non-btrfs filesystem, `braid ack` returns a probe error naming the fstype and preserves `alert-latch.json`, `smartd-alert`, and `acked-stats.json`.
- `docs/decisions/014-alerts.md` -- in the corrupt-latch and offline-ack policy sections, qualify the cleanup guarantee to mounted or genuinely unmounted ack paths. Add the same invariant: only a genuinely unmounted pool uses offline ack. A foreign fstype is not an offline pool state for `cmd_ack`; it is a probe error and must not clear or mutate alert state, including corrupt latch bytes.
  Because this substantively edits an existing doc, also add YAML frontmatter with a required `intent` field per `docs/index.md`, while preserving the existing `Status: Active` line.

## Files modified

- `cli/src/ack.rs`
  - Drop `NotBtrfs` arm in `cmd_ack`.
  - Change the unreadable-latch warning to state-neutral "treating as active for ack gating" wording.
  - Refactor body into `cmd_ack_impl`; thread `&dyn Fn()` beeper hook through to `ack_offline`.
  - Keep the cfg-split `stop_beeper` (cfg(not(test)) real systemctl, cfg(test) no-op); add `cmd_ack_impl` so selected tests can inject a recording beeper closure while existing public-`cmd_ack` tests stay host-safe.
  - Preserve `format_systemctl_stop_failure` and its existing tests unchanged.
  - Add `NotMountedFs` mock; migrate 7 `ack_offline_*` tests from `&Ext4Fs` to `&NotMountedFs`.
  - Add 3 new public `cmd_ack` `NotBtrfs` behavior tests (with-alerts, no-alerts, corrupt latch).
  - Add 1 `cmd_ack_impl` beeper non-invocation test for the `NotBtrfs` path.
  - Update 1 offline-ack test to assert beeper fires once via injected closure.
- `manual/commands/ack.md` -- document that foreign fstype is a preserving probe error, not offline ack.
- `docs/decisions/014-alerts.md` -- add required YAML `intent` frontmatter; qualify corrupt-latch cleanup to mounted/genuinely-unmounted ack paths; record the same foreign-fstype invariant in the offline ack policy.

`main.rs:664` is unaffected.

## Out of scope (intentional)

- The same `NotBtrfs ⇒ treated-as-offline` pattern exists in `remove.rs:247`, `replace.rs:652`, `remove_missing.rs:280`, and `status.rs:349`. They have weaker consequences (no latch deletion / no acked-stats mutation / `status` is read-only) and a different UX contract. Out of scope here; revisit separately.
- `monitor.rs:63` also treats `ProbeError::NotBtrfs` as offline, and `monitor_classifies_non_btrfs_mount_as_offline` intentionally pins that behavior for the headless monitor surface. Out of scope here; do not change it as part of the ack fix.

## Verification

End-to-end checks, in order:

1. `just test-rust` -- the 7 migrated `ack_offline_*` tests must remain green (they now exercise the genuine `pool.mounted == false` branch, not the `NotBtrfs` shortcut). The 3 new public `cmd_ack_with_foreign_fstype_*` tests must pass, including corrupt-latch byte preservation. The new hook-only `cmd_ack_impl_with_foreign_fstype_does_not_invoke_beeper` test must observe zero `stop_beeper` invocations. The migrated offline-ack beeper-counter test must observe exactly one `stop_beeper` invocation. The existing `format_systemctl_stop_failure_*` tests must remain green, proving the refactor preserved the non-zero systemctl warning behavior. Tests are self-contained (tempdir state, mocked filesystem, recorded beeper) and never touch real systemd.
2. `just test-vm monitor-lifecycle` -- gates the production wiring of `stop_beeper` on the mounted-ack path. The injection refactor changes how the public `cmd_ack` reaches `systemctl stop braid-alert.service`; this VM test asserts `braid ack` deactivates `braid-alert.service` and removes the latch end-to-end (`tests/module/monitor-lifecycle.py:78-81`). Required because `just test-rust` only exercises the closure, not the real systemctl call. (The migrated unit-test counter pins the offline-ack invocation; this VM test pins the mounted-ack invocation.)
3. Manual sanity check (only if a VM is already up): with `/mnt/storage` mounted as ext4 and a stale alert latch present, run `braid ack` and confirm the user sees `error: probe error: /mnt/storage is mounted but fstype is ext4, not btrfs` (exit 1) and that `/var/lib/braid/alert-latch.json` still exists afterward. Skip otherwise; the unit tests pin the same behavior.
