# Plan: Fix Three Alert System Bugs

Status: Draft

## Context

Code review of the alert implementation found three bugs. Two are blocking — they break the reliability of the alert/ack workflow. The third is a correctness issue that makes debugging harder.

1. **Monitor exit code conflation** — `braid monitor` exits 1 for both "alert active" and "internal failure" (probe/parse/config error), so the systemd wrapper falsely triggers the beeper on operational errors.
2. **Latched alerts unusable while pool offline** — alerts are latched (persist until ack), but `braid status` hardcodes `alert_active: false` when pool is not mounted, and `braid ack` refuses to run. A latched beeper persists with no usable status/ack path after the pool is locked.
3. **Unmapped device paths become devid 0** — when a btrfs device stats path isn't in the devid map, the code silently synthesizes devid 0, creating bogus alert causes. The fix must fail closed — surface the error, don't hide it.

## Fix 1: Monitor exit codes

Reserve exit 1 for "alert active" and exit 2 for "monitor execution failure." The systemd wrapper only starts the beeper on exit 1.

### `cli/src/main.rs` (~line 379-402)

Change the two error paths to exit 2:
- Config read error (line 384): `std::process::exit(1)` → `std::process::exit(2)`
- `cmd_monitor` `Err(e)` (line 400): `std::process::exit(1)` → `std::process::exit(2)`

Update the doc comment on the `Monitor` variant (line 43):
```rust
/// Check disk health: exit 0 = ok/offline, exit 1 = alert, exit 2 = error
Monitor,
```

### `modules/braid/monitor.nix` (~line 50-55)

Replace the if/else with exit code capture:
```nix
script = ''
  rc=0
  braid monitor || rc=$?
  if [ "$rc" -eq 1 ]; then
    ${pkgs.systemd}/bin/systemctl start braid-alert.service 2>/dev/null || true
  elif [ "$rc" -ge 2 ]; then
    echo "braid monitor failed (exit $rc)" >&2
  fi
  exit 0
'';
```

### `docs/decisions/alerts.md`

Add exit codes to the "`braid monitor` is a pure detector" section: 0 = ok/offline, 1 = alert, 2 = error.

### `tests/cli/braid-monitor.py`

Add a subtest that captures the exact exit code on a degraded pool and asserts it equals 1 (not 2):
```python
with subtest("Degraded pool: monitor exit code is exactly 1"):
    rc = machine.succeed("braid monitor; echo $? || echo $?").strip().split('\n')[-1]
    assert rc == "1", f"Expected exit 1, got {rc}"
```

## Fix 2: Latched alerts work while pool offline

### Problem

Alerts are latched — they persist until `braid ack`. The beeper can be started by either `braid monitor` (btrfs alerts) or the smartd exec script (SMART alerts). But when the pool goes offline, the current code has no way to show or acknowledge latched alerts.

### Solution: alert latch file

Persist the `AlertState` to `/var/lib/braid/alert-latch.json` when an alert is detected. Offline status and ack read this file. **Only `braid ack` removes the latch** — this is the core invariant. `braid monitor` creates or refreshes the latch but never clears it, matching the "latched until ack" design.

**Writers:**
- `braid monitor` — writes (creates or refreshes) latch when alert is active. Never removes it.
- smartd exec script — already creates the smartd flag file, which `ack_offline` checks independently.

**Readers:**
- `braid status` (offline) — reads latch file, shows banner if present and active
- `braid ack` (offline) — reads latch file to know what to acknowledge

**Cleared by:**
- `braid ack` — the sole mechanism that removes `alert-latch.json` (both online and offline paths)

### `cli/src/alert.rs`

Add a new constant and load/save/remove functions for the latch file:

```rust
pub const ALERT_LATCH_FILE: &str = "/var/lib/braid/alert-latch.json";

pub fn load_alert_latch() -> Option<AlertState> {
    let contents = std::fs::read_to_string(ALERT_LATCH_FILE).ok()?;
    serde_json::from_str(&contents).ok()
}

pub fn save_alert_latch(state: &AlertState) -> Result<(), std::io::Error> {
    let json = serde_json::to_string_pretty(state).map_err(std::io::Error::other)?;
    atomic_write(Path::new(ALERT_LATCH_FILE), json.as_bytes())
}

pub fn remove_alert_latch() -> Result<(), std::io::Error> {
    match std::fs::remove_file(ALERT_LATCH_FILE) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}
```

### `cli/src/monitor.rs` — write latch (never clear)

After computing alert state (line ~84), replace the existing if/else:

```rust
if alert_state.active {
    if let Err(e) = alert::save_alert_latch(&alert_state) {
        eprintln!("Warning: failed to write alert latch: {e}");
    }
    Ok(MonitorResult::Alert(alert_state))
} else {
    Ok(MonitorResult::Ok)
}
```

Monitor does NOT remove the latch when alert clears. The latch persists until `braid ack`. This means:
- Alert detected → latch written → beeper starts
- Condition resolves (e.g., drive replaced) → monitor returns Ok → beeper still running (latched)
- User runs `braid ack` → latch removed, acked baseline updated, beeper stopped
- Next monitor run → no alert (baseline matches current) → no new latch

### `cli/src/status.rs` — latch is authoritative in ALL paths

The latch is the authority for "is there an unacknowledged alert?" in every surface — online and offline. This is the core change: a resolved-but-unacked alert still shows in status because the latch persists.

**Replace `compute_alert_for_pool()`** (line ~521) with a latch-based function:

```rust
fn resolve_alert_state() -> AlertState {
    let latch = alert::load_alert_latch();
    let smartd_active = alert::smartd_alert_active();

    match latch {
        Some(mut state) if state.active => {
            // Latch is active — use it. Also check smartd flag for
            // between-cycle fires not yet in the latch.
            if smartd_active
                && !state.causes.iter().any(|c| matches!(c, AlertCause::SmartdAlert))
            {
                state.causes.push(AlertCause::SmartdAlert);
            }
            state
        }
        _ if smartd_active => AlertState {
            active: true,
            causes: vec![AlertCause::SmartdAlert],
        },
        _ => AlertState {
            active: false,
            causes: vec![],
        },
    }
}
```

This replaces both the online `compute_alert_for_pool(&pool, &device_stats)` calls (lines ~356 and ~456) AND the three not-mounted early returns. All six sites call the same `resolve_alert_state()`.

Since alert state no longer comes from live data, `compute_alert_for_pool` is removed. The `get_device_stats` call in `cmd_status` can move back into the verbose-only block (it's still needed for `build_disk_reports`). In `build_status_report`, device stats are already fetched for disk reports — unchanged.

### `cli/src/tui/probe.rs` — latch-based alert

Replace the existing live alert computation (lines ~166-181) with the same latch read:

```rust
let alert_state = resolve_alert_state_for_tui();
```

Where `resolve_alert_state_for_tui` uses the same latch + smartd logic. (Import `resolve_alert_state` from status.rs or duplicate the small function in the TUI probe — either works, preference for import to avoid duplication.)

### `cli/src/ack.rs` — offline ack via latch

Replace the two not-mounted early returns (`NotBtrfs` and `!pool.mounted`) with calls to `ack_offline()`:

```rust
fn ack_offline() -> Result<(), AckError> {
    let latch = alert::load_alert_latch();
    let smartd_active = alert::smartd_alert_active();

    let has_alert = latch.as_ref().map_or(false, |s| s.active) || smartd_active;
    if !has_alert {
        return Err(AckError::PoolNotMounted);
    }

    alert::remove_alert_latch()?;
    alert::remove_smartd_alert_flag()?;
    stop_beeper();
    println!("acknowledged current alerts");
    Ok(())
}
```

Also update the online ack path (end of `cmd_ack`, after `save_acked_stats` and `remove_smartd_alert_flag`) to remove the latch file:
```rust
alert::remove_alert_latch()?;
```

### `tests/cli/braid-monitor.py` — latch assertions

Add inline assertions to existing subtests:
- After "monitor exits 1" (step 5): verify `test -f /var/lib/braid/alert-latch.json` exists
- After "ack clears alert" (step 8): verify `test -f /var/lib/braid/alert-latch.json` does NOT exist

### `tests/cli/braid-smartd-alert.py` — offline subtests

Add at the end (before `machine.shutdown()`):

```python
with subtest("Pool offline with smartd alert: status shows ALERT"):
    machine.succeed("umount /mnt/storage")
    machine.succeed("cryptsetup close braid-disk1")
    machine.succeed("cryptsetup close braid-disk2")
    machine.succeed("touch /var/lib/braid/smartd-alert")
    output = machine.succeed("braid status")
    assert "ALERT" in output
    assert "SMART" in output

with subtest("Pool offline with smartd alert: ack succeeds"):
    machine.succeed("braid ack")
    machine.fail("test -f /var/lib/braid/smartd-alert")

with subtest("Pool offline with no alert: ack fails"):
    machine.fail("braid ack")
```

Add a new test for btrfs-derived offline alerts in `tests/cli/braid-monitor.py`:

```python
with subtest("Btrfs alert latched after pool offline"):
    # Pool is still degraded. Remove acked state to re-trigger alert.
    machine.succeed("rm -f /var/lib/braid/acked-stats.json")
    machine.fail("braid monitor")
    machine.succeed("test -f /var/lib/braid/alert-latch.json")
    # Now lock the pool
    machine.succeed("umount /mnt/storage")
    machine.succeed("cryptsetup close braid-disk1")
    machine.succeed("cryptsetup close braid-disk3")
    # Status should still show the latched alert
    output = machine.succeed("braid status")
    assert "ALERT" in output
    # Offline ack should succeed
    machine.succeed("braid ack")
    machine.fail("test -f /var/lib/braid/alert-latch.json")
```

## Fix 3: Unmapped device paths are a hard error

An unmapped device path means the devid map is incomplete — a bug. Fail closed: return an error. Callers surface it visibly, not silently.

### `cli/src/alert.rs` — change return types to `Result`

Add error type:
```rust
#[derive(Debug, thiserror::Error)]
#[error("device {path} not found in devid map")]
pub struct UnmappedDeviceError {
    pub path: String,
}
```

Change `compute_alert_state_with_devid_map` signature:
```rust
pub fn compute_alert_state_with_devid_map(
    ...
) -> Result<AlertState, UnmappedDeviceError>
```

Replace the `unwrap_or(0)` (line 100) with:
```rust
let devid = *path_to_devid.get(&dev.device_path).ok_or_else(|| {
    UnmappedDeviceError { path: dev.device_path.clone() }
})?;
```

Change `snapshot_current` similarly:
```rust
pub fn snapshot_current(...) -> Result<AckedStats, UnmappedDeviceError>
```

### `cli/src/alert.rs` — add `ComputationError` cause variant

Add a new variant to `AlertCause` for surfacing computation failures as visible alerts (fail closed):

```rust
pub enum AlertCause {
    BtrfsDeviceErrors { devid: u64 },
    MissingDevice { devid: u64 },
    SmartdAlert,
    ComputationError { detail: String },
}
```

### Caller updates

The latch-authoritative model (Fix 2) means `braid monitor` is the sole writer and the place where `ComputationError` is surfaced. Status and TUI are pure readers of the latch — they never call `compute_alert_state_with_devid_map` directly.

**`cli/src/monitor.rs`** — handle `UnmappedDeviceError` by writing a `ComputationError` latch:

```rust
// After building path_to_devid, computing alert state:
let alert_state = match compute_alert_state_with_devid_map(...) {
    Ok(state) => state,
    Err(e) => {
        eprintln!("error: {e}");
        // Fail closed: write a ComputationError latch so the user sees it
        let error_state = AlertState {
            active: true,
            causes: vec![AlertCause::ComputationError {
                detail: e.to_string(),
            }],
        };
        if let Err(write_err) = alert::save_alert_latch(&error_state) {
            eprintln!("Warning: failed to write alert latch: {write_err}");
        }
        return Err(MonitorError::UnmappedDevice(e));
    }
};
```

Add `UnmappedDeviceError` to `MonitorError`:
```rust
#[error("unmapped device: {0}")]
UnmappedDevice(#[from] crate::alert::UnmappedDeviceError),
```

This way: mapping error → latch written with `ComputationError` cause → exit 2 (not beep-worthy via Fix 1, but visible in status/TUI via the latch). The user sees the problem in status and can `braid ack` to clear it after investigating.

**`cli/src/ack.rs`** — `snapshot_current` now returns `Result`. Propagate with `?`:
```rust
#[error("unmapped device: {0}")]
UnmappedDevice(#[from] crate::alert::UnmappedDeviceError),
```

**`cli/src/status.rs`** — `compute_alert_for_pool` is removed entirely (per Fix 2). Status reads the latch via `resolve_alert_state()`. No live alert computation, no `UnmappedDeviceError` handling needed.

**`cli/src/tui/probe.rs`** — same: reads latch, no live alert computation, no error handling needed.

**`cli/src/status.rs` `format_status_human`** and **`cli/src/tui/view/mod.rs`** — add rendering for `ComputationError` in the alert banner:
```rust
AlertCause::ComputationError { ref detail } => {
    out.push_str(&format!("  - alert computation error: {detail}\n"));
}
```

### Unit tests

Update existing tests to unwrap the new `Result`. Add:

```rust
#[test]
fn unmapped_device_is_error_in_alert() {
    let mut dev = zero_device("/dev/mapper/braid-unknown");
    dev.read_io_errs = 5;
    let stats = make_stats(vec![dev]);
    let acked = AckedStats::default();
    let map = devid_map(&[]);
    let result = compute_alert_state_with_devid_map(&stats, &acked, &[], false, &map);
    assert!(result.is_err());
}

#[test]
fn unmapped_device_is_error_in_snapshot() {
    let dev = zero_device("/dev/mapper/braid-unknown");
    let stats = make_stats(vec![dev]);
    let map = devid_map(&[]);
    let result = snapshot_current(&stats, &[], &map);
    assert!(result.is_err());
}
```

## Source comments

Add short comments at these non-obvious design points:

1. **`cli/src/alert.rs`** at `ALERT_LATCH_FILE` constant: explain that the latch file is the authoritative source of "is there an unacknowledged alert?" for all UI surfaces (status, TUI, offline). Monitor writes it; only ack clears it.

2. **`cli/src/monitor.rs`** at the latch-write block: explain why monitor never removes the latch — alerts are latched until `braid ack`, even if the triggering condition resolves. This prevents resolved-but-unacked alerts from disappearing.

3. **`cli/src/monitor.rs`** at the `ComputationError` latch-write in the `Err(e)` arm: explain why it writes a `ComputationError` latch AND returns exit 2. The latch makes the error visible in status/TUI (fail closed for display). Exit 2 prevents the beeper from starting (it's an operational error, not a confirmed disk health alert).

4. **`cli/src/status.rs`** at `resolve_alert_state()`: explain why status reads the latch instead of recomputing live alert state. The latch is the single source of truth — recomputing would cause the alert to disappear when a condition resolves, contradicting the "latched until ack" model. The smartd flag is checked as a bridge for between-cycle fires.

## Order

1. Fix 3 (devid mapping error) — pure Rust, changes return types that other fixes depend on
2. Fix 1 (exit codes) — Rust + Nix, uses the error propagation from Fix 3
3. Fix 2 (latch file for offline alerts) — Rust + tests, builds on clean error handling

## Verification

1. `just test-rust` after each fix — all unit tests pass
2. `just test braid-monitor` — exit 1 only on real alert; exit 2 on error; latch created on alert, persists until ack
3. `just test braid-smartd-alert` — offline status shows alert; offline ack clears flag + latch + stops beeper
