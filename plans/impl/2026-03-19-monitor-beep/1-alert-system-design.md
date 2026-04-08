# Plan: First-Class Alerts for Disk Health

Status: Draft

## Context

Synology NAS boxes beep when a disk develops bad sectors — you hear it, SSH in, and deal with it. Without this, a braid NAS user has no idea anything is wrong unless they happen to run `braid status`. This plan introduces **Alerts** as a first-class concept in braid: a shared alert model that all surfaces consume (`braid status`, TUI, beeper), with two detection sources (btrfs device stats + SMART via smartd) and a single acknowledgment workflow (`braid ack`).

## Design Decisions

- **Alert is the primary domain concept**: braid has first-class Alerts. Beeping is one notification mechanism for an active alert. `braid status` is the primary place to understand active alerts. `braid ack` acknowledges current alerts and silences notifications.
- **Shared alert computation**: a single `compute_alert_state()` function produces an `AlertState` consumed by all surfaces — `braid monitor` (exit code), `braid status` (banner + causes), TUI (banner + indicators). No surface re-encodes alert logic.
- **Alert causes are explicit**: `AlertCause` enum — `BtrfsDeviceErrors`, `MissingDevice`, `SmartdAlert`. The banner is cause-neutral ("disk health issue detected"); cause details appear below it and in JSON output.
- **Two detection sources, one alert model**: braid owns btrfs device stats + missing device detection. smartd owns SMART monitoring and writes a flag file (`/var/lib/braid/smartd-alert`) when triggered. `compute_alert_state()` checks both sources.
- **All five btrfs device stat counters trigger alerts**: write_io_errs, read_io_errs, flush_io_errs, corruption_errs, generation_errs. Any non-zero counter above acked baseline → alert.
- **`braid ack` acknowledges alerts**: the user action is "acknowledge alert." Side effects: stop beeping, clear smartd flag, update acked baseline. The monitor won't re-trigger for the same condition. If a *different* issue occurs after ack, that's a new alert.
- **Ack state keyed by btrfs devid**: devid is btrfs-native — no cross-referencing config or disk-map. The parser already sees missing device devids; we just need to stop discarding them.
- **Ack state is machine-local, not pool-portable**: on a new machine, state doesn't exist → everything evaluates fresh.
- **Ack state separate from disk-map.json**: different concerns (identity vs acknowledgment), different write patterns, different risk profiles (precious vs disposable).
- **`braid monitor` is a pure detector**: checks state, returns exit 0 (ok) or exit 1 (alert). Does not start/stop services. The systemd wrapper starts the beeper on exit 1.
- **Alerts are latched**: persist until `braid ack` — even if the triggering condition disappears. "Something happened that needs acknowledgment," not "something is currently true." Avoids the cross-source bug where btrfs monitor exit 0 could clear a smartd-triggered alert. Matches Synology UX.
- **Periodic one-shot, not daemon**: systemd timer + oneshot service. No mount condition on timer — `braid monitor` handles pool-not-mounted gracefully (exit 0).
- **Monitor self-heals ack state**: after drive replacement, resets `missing_acked` for now-present devids. No coupling to replace/add commands.
- **On by default**: `braid.monitor.enable` defaults to true when `braid.enable` is true. beep/pcspkr failures silently swallowed.

## Implementation

### Step 1: Alert model + acked state (Rust)

New file: `cli/src/alert.rs`

#### Alert model

All surfaces consume the same `AlertState`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertState {
    pub active: bool,
    pub causes: Vec<AlertCause>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AlertCause {
    BtrfsDeviceErrors { devid: u64 },
    MissingDevice { devid: u64 },
    SmartdAlert,
}
```

#### Shared computation

```rust
pub fn compute_alert_state(
    current_stats: &BtrfsDeviceStatsOutput,
    acked: &AckedStats,
    missing_devids: &[u64],
    smartd_alert_active: bool,
) -> AlertState
```

- For each device: if any current counter > acked counter → `BtrfsDeviceErrors { devid }`
- Counter reset detection: if current < acked, treat acked as 0 (remount reset)
- Missing device not acked → `MissingDevice { devid }`
- smartd alert flag file exists → `SmartdAlert`
- `active = !causes.is_empty()`

This single function is called by `braid monitor`, `braid status`, and TUI.

#### Acked state

Keyed by btrfs devid. Clean map with no mixed top-level fields:

```rust
/// Keyed by btrfs devid (e.g. "1", "2")
pub struct AckedStats(pub BTreeMap<String, AckedDisk>);

pub struct AckedDisk {
    pub missing_acked: bool,
    pub device_stats: AckedDeviceCounters,
}

pub struct AckedDeviceCounters {
    pub read_io_errs: u64,
    pub write_io_errs: u64,
    pub flush_io_errs: u64,
    pub corruption_errs: u64,
    pub generation_errs: u64,
}
```

On-disk format (`/var/lib/braid/acked-stats.json`):

```json
{
  "1": {
    "missing_acked": false,
    "device_stats": {
      "read_io_errs": 3,
      "write_io_errs": 0,
      "flush_io_errs": 0,
      "corruption_errs": 1,
      "generation_errs": 0
    }
  }
}
```

#### smartd alert flag

`/var/lib/braid/smartd-alert` — the flag file IS the state. No tracking in AckedStats:
- smartd exec script touches the file to signal an alert
- `compute_alert_state()` checks if the file exists → `SmartdAlert` cause
- `braid ack` removes the file
- Latch works: flag persists until ack. Re-alert works: a new smartd touch recreates it after ack removed it.
- Follows `disk_map.rs` patterns: `load_acked_stats()` / `save_acked_stats()` using `atomic_write` from `state_io.rs`
- Returns empty struct on missing/corrupt file
- `snapshot_current(current_stats, missing_devids) -> AckedStats` — captures current values + sets `missing_acked: true` for missing devids
- Register in `cli/src/lib.rs`
- Unit tests: roundtrip, compute_alert_state with each cause type, counter reset, missing ack, smartd flag, empty file

Reuses:
- `state_io::atomic_write` (`cli/src/state_io.rs`)
- `DeviceErrorStats` from `cli/src/parse/types.rs`
- `BtrfsDeviceStatsOutput` from `cli/src/parse/types.rs`

### Step 2: Expose missing devids from parser

Currently `parse_btrfs_filesystem_show()` sees missing device devids (e.g. `devid 2 size 0 used 0 path /dev/mapper/braid-vdb MISSING`) but filters them out. Capture them instead.

In `cli/src/parse/btrfs_filesystem_show.rs`:
- Instead of discarding MISSING lines, collect their devids into a new field
- Add `missing_devids: Vec<u64>` to `BtrfsFilesystemShowOutput` in `cli/src/parse/types.rs`

In `cli/src/probe.rs`:
- Expose `missing_devids` on `PoolState` (pass through from parser output)

### Step 3: `braid monitor` command (Rust)

New file: `cli/src/monitor.rs`

`braid monitor` is a **pure detector** — it checks state and returns an exit code. It does not start/stop services. (It may write the ack file for self-heal of stale state.)

`cmd_monitor(runner, fs, config) -> Result<MonitorResult>`

1. Check if pool is mounted (via `probe_pool`). If not → return `MonitorResult::PoolOffline` (exit 0).
2. Run `btrfs device stats` (existing `CmdRequest::BtrfsDeviceStats`).
3. Load acked stats.
4. Get missing devids (from pool probe's new `missing_devids` field).
5. Check smartd alert flag (`/var/lib/braid/smartd-alert` exists).
6. **Self-heal stale ack state**: for any devid where `missing_acked: true` but the devid is now present, reset `missing_acked` to false. Save updated ack file if changed.
7. Call `compute_alert_state()`.
8. Return exit code based on `alert_state.active`.

Exit codes:
- 0: no alert (or pool offline)
- 1: alert active

Wire in `cli/src/main.rs`:
- Add `Monitor` to `Commands` enum (no args)
- Map result to process exit code

Reuses:
- `CmdRequest::BtrfsDeviceStats` (`cli/src/cmd.rs`)
- `parse_btrfs_device_stats` (`cli/src/parse/btrfs_device_stats.rs`)
- `probe_pool` (`cli/src/probe.rs`)

### Step 4: `braid ack` command (Rust)

New file: `cli/src/ack.rs`

`braid ack` **acknowledges current alerts**. Silencing the beeper is a side effect.

`cmd_ack(runner, config) -> Result<()>`

1. Check if pool is mounted. If not → error.
2. Run `btrfs device stats`.
3. Get missing devids (from pool probe).
4. `snapshot_current(stats, missing_devids)` → save to acked stats file (sets `missing_acked: true` for missing devids).
5. Remove smartd alert flag (`/var/lib/braid/smartd-alert`) if present — this is the sole ack mechanism for smartd alerts.
6. Stop beeper: `systemctl stop braid-alert.service` — best-effort, warn on failure. Uses the systemd binary from the wrapper PATH (see Step 5 — systemd added to `toolPackages`).
7. Print confirmation: "acknowledged N alert(s)"

Wire in `cli/src/main.rs`:
- Add `Ack` to `Commands` enum (no args)
- Help text: "Acknowledge current alerts and silence notifications"

### Step 5: NixOS module — monitor and alert services

New file: `modules/braid/monitor.nix` (import from `default.nix`)

#### New options in `options.nix`:

```nix
braid.monitor = {
  enable = lib.mkEnableOption "disk health monitoring and alerting" // { default = true; };
  interval = lib.mkOption {
    type = lib.types.str;
    default = "5min";
    description = "Polling interval for btrfs device stats (systemd time span, e.g. \"5min\", \"30s\").";
  };
  alertCommand = lib.mkOption {
    type = lib.types.nullOr lib.types.str;
    default = null;
    description = "Custom command to run on alert, in addition to beep.";
  };
};
```

#### `braid-alert.service` — the beeper

```nix
systemd.services.braid-alert = {
  description = "Braid disk health alert (audible beep)";
  serviceConfig.Type = "simple";
  script = ''
    ${pkgs.kmod}/bin/modprobe pcspkr 2>/dev/null || true
    ${lib.optionalString (cfg.monitor.alertCommand != null) ''
      ${cfg.monitor.alertCommand} || true
    ''}
    while true; do
      ${pkgs.beep}/bin/beep -f 1000 -l 500 2>/dev/null || true
      sleep 5
    done
  '';
};
```

- `systemctl stop` sends SIGTERM → kills loop → silence
- `beep` failure swallowed (no pcspkr hardware = silent degradation)
- Custom alert command runs once at start, beep loops continuously
- Single beep pattern — `braid status` provides the details, not the beep

#### `braid-monitor.service` + timer

The wrapper starts the alert on exit 1, does nothing on exit 0. Alerts are latched — `braid ack` is the sole clear path.

```nix
systemd.services.braid-monitor = {
  description = "Poll btrfs device stats for disk errors";
  serviceConfig.Type = "oneshot";
  path = [ braidWrapped cfg.packages.btrfsProgs ];
  script = ''
    # Capture exit code without triggering NixOS set -e / fail-fast
    if braid monitor; then true; else
      ${pkgs.systemd}/bin/systemctl start braid-alert.service 2>/dev/null || true
    fi
    exit 0
  '';
};

systemd.timers.braid-monitor = {
  description = "Periodic braid disk health check";
  wantedBy = [ "timers.target" ];
  timerConfig = {
    OnActiveSec = cfg.monitor.interval;
    OnUnitActiveSec = cfg.monitor.interval;
  };
};
```

- Non-fatal exit capture: `if/else` prevents NixOS script `set -e` from killing the wrapper on non-zero exit
- Exit 0 does nothing: alerts are latched and persist until `braid ack`
- Exit 1 starts alert: uses `start` (not `restart`) — if alert is already running, this is a no-op, which is correct for latched alerts
- No `ConditionPathIsMountPoint` on the timer — `braid monitor` handles pool-not-mounted gracefully (exit 0)

### Step 6: NixOS module — smartd integration

In `monitor.nix`:

```nix
services.smartd = {
  enable = lib.mkDefault true;
  defaults.monitored = lib.mkDefault
    "-a -o on -S on -m root -M exec ${smartdAlertScript}";
  notifications.wall.enable = lib.mkDefault false;
};
```

Where `smartdAlertScript` is:

```nix
smartdAlertScript = pkgs.writeShellScript "braid-smartd-alert" ''
  touch /var/lib/braid/smartd-alert
  ${pkgs.systemd}/bin/systemctl start braid-alert.service 2>/dev/null || true
'';
```

smartd fires the exec script on SMART attribute changes. The flag file `/var/lib/braid/smartd-alert` is the bridge into braid's alert model — `compute_alert_state()` checks for its existence, `braid ack` removes it. The flag persists across reboots (SMART issues don't resolve on reboot).

### Step 7: `braid status` enhancements

In `cli/src/status.rs`:

**Alert banner**: `braid status` calls `compute_alert_state()` (same as monitor and TUI). If alert is active, print a cause-neutral banner at the top, followed by cause details:

```
ALERT -- disk health issue detected. Run 'braid ack' to acknowledge and silence.
  - missing device (devid 2)
  - btrfs device errors on devid 1
```

Or for smartd-only:

```
ALERT -- disk health issue detected. Run 'braid ack' to acknowledge and silence.
  - SMART health warning
```

Disappears after `braid ack` (acked baseline matches current state).

**JSON output**: add `AlertState` to `StatusReport`:

```json
{
  "alert_active": true,
  "alert_causes": [
    {"type": "missing_device", "devid": 2},
    {"type": "btrfs_device_errors", "devid": 1}
  ],
  ...
}
```

When no alert: `"alert_active": false, "alert_causes": []`.

**Per-disk action guidance** (below the existing error line):
- When a disk has non-zero errors:
  ```
  Errors:  read 3 / write 0 / flush 0 / corruption 1 / generation 0
  Action:  add replacement disk to config, then: braid replace --old <name> --new <new-name>
  ```
- When a disk is missing:
  ```
  Status:  MISSING
  Action:  add replacement disk to config, then: braid replace --old <name> --new <new-name>
  ```
- Follows config-first workflow: replacement disks must be declared in `braid.disks` before `braid replace` can use them.

### Step 8: TUI enhancements

- **Alert banner**: TUI calls `compute_alert_state()`. When active, show a persistent banner at the top (red background): `ALERT -- disk health issue detected. Run 'braid ack' to acknowledge and silence.`
- Add `alert_state: AlertState` to `PoolState` in `cli/src/tui/model.rs`
- Add `device_errors: HashMap<String, DiskErrors>` to `PoolState`
- Probe device errors + acked stats + smartd flag in `cli/src/tui/probe.rs` (calls `compute_alert_state()`)
- Disk table in `cli/src/tui/view/mod.rs`: show error total per disk, red when non-zero
- Disk detail popup: show all five error counters
- Update snapshot tests with alert + error data

### Step 9: NixOS VM tests

#### Test: btrfs alert lifecycle (`tests/cli/braid-monitor.py`)

Intent: Verify the full alert lifecycle for btrfs-detected issues: detection → banner → ack → cleared.

Scenario:
1. Create 3-disk RAID1 pool
2. `braid monitor` → exit 0 (no alerts)
3. `braid status` → output does NOT contain "ALERT"
4. Close one LUKS mapper, remount degraded → missing device
5. `braid monitor` → exit 1 (alert active)
6. `braid status` → output contains "ALERT" banner, "braid ack", and "missing device"
7. `braid status --json` → JSON contains `"alert_active": true` and `"missing_device"` cause
8. `braid ack` → verify acked state file written, contains `missing_acked: true` for the missing devid
9. `braid status` → output does NOT contain "ALERT"
10. `braid monitor` → exit 0 (acknowledged, no re-trigger)

#### Test: smartd alert lifecycle (`tests/cli/braid-smartd-alert.py`)

Intent: Verify smartd-triggered alerts appear in `braid status` and clear with `braid ack`.

Scenario:
1. Create 2-disk RAID1 pool (healthy)
2. `braid status` → no "ALERT"
3. Simulate smartd alert: `touch /var/lib/braid/smartd-alert`
4. `braid monitor` → exit 1
5. `braid status` → output contains "ALERT" and "SMART"
6. `braid ack` → verify smartd flag removed
7. `braid status` → no "ALERT"
8. `braid monitor` → exit 0

#### Test: alert service lifecycle (`tests/module/braid-alert.py`)

Intent: Verify systemd units exist and can be started/stopped.

1. Enable `braid.monitor`
2. Verify `braid-monitor.timer` is active
3. Verify `braid-alert.service` unit exists
4. `systemctl start braid-alert.service` → verify active → `systemctl stop` → verify inactive

### Step 10: Docs

- Update `README.md` with Alerts section: braid has first-class Alerts. Beeping is the default audible notifier. `braid status` is the primary place to understand active alerts. `braid ack` acknowledges current alerts and silences notifications.
- Add `docs/decisions/014-alerts.md` (ADR, status: Active): first-class alert concept, shared `compute_alert_state()`, alert causes, latched alerts, ack workflow, two detection sources.

## Files to create

| File | Purpose |
|------|---------|
| `cli/src/alert.rs` | Alert model, `compute_alert_state()`, acked state management |
| `cli/src/monitor.rs` | `braid monitor` command |
| `cli/src/ack.rs` | `braid ack` command |
| `modules/braid/monitor.nix` | NixOS services/timer/smartd |
| `tests/cli/braid-monitor.py` | VM test: btrfs alert lifecycle |
| `tests/cli/braid-monitor.nix` | VM test: NixOS config |
| `tests/cli/braid-smartd-alert.py` | VM test: smartd alert lifecycle |
| `tests/cli/braid-smartd-alert.nix` | VM test: NixOS config |
| `tests/module/braid-alert.py` | VM test: alert service |
| `tests/module/braid-alert.nix` | VM test: NixOS config |
| `docs/decisions/014-alerts.md` | ADR: first-class alerts |

## Files to modify

| File | Change |
|------|--------|
| `cli/src/lib.rs` | Add `pub mod alert;`, `pub mod monitor;`, `pub mod ack;` |
| `cli/src/main.rs` | Add `Monitor` and `Ack` to `Commands` enum |
| `cli/src/parse/types.rs` | Add `missing_devids: Vec<u64>` to `BtrfsFilesystemShowOutput` |
| `cli/src/parse/btrfs_filesystem_show.rs` | Capture missing devids instead of discarding them |
| `cli/src/probe.rs` | Expose `missing_devids` on `PoolState` |
| `cli/src/status.rs` | Add action guidance text for errors/missing |
| `cli/src/tui/model.rs` | Add `device_errors` to `PoolState` |
| `cli/src/tui/probe.rs` | Poll device stats |
| `cli/src/tui/view/mod.rs` | Show errors in disk table + detail |
| `modules/braid/options.nix` | Add `braid.monitor.*` options |
| `modules/braid/wrapper.nix` | Add `pkgs.systemd` to `toolPackages` (needed for `braid ack` to call `systemctl`) |
| `modules/braid/default.nix` | Import `monitor.nix` |
| `README.md` | Monitoring section |

## Verification

1. `just test-rust` — unit tests for `compute_alert_state()`, acked stats, each cause type
2. `just test braid-monitor` — VM test: btrfs alert lifecycle (detection → status banner → ack → cleared)
3. `just test braid-smartd-alert` — VM test: smartd alert lifecycle (flag → status banner → ack → cleared)
4. `just test braid-alert` — VM test: systemd service lifecycle
5. Manual: on a NixOS machine with `braid.monitor.enable = true`, verify `systemctl list-timers` shows `braid-monitor.timer`, `braid monitor` exits 0 on healthy pool, `braid status` shows no alert banner
