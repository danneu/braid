# Plan: Disk Health Monitoring & Audible Alerting

Status: Draft

## Context

Synology NAS boxes beep when a disk develops bad sectors — you hear it, SSH in, and deal with it. Without this, a braid NAS user has no idea anything is wrong unless they happen to run `braid status`. This plan adds two detection layers (btrfs device stats + SMART via smartd) that feed a single alert path (PC speaker beep), with an ack workflow to silence it.

## Design Decisions

- **Two detection layers, one alert path**: braid owns btrfs device stats polling. smartd (already a NixOS service) owns SMART monitoring. Both trigger the same `braid-alert.service`.
- **All five btrfs device stat counters are beep-worthy**: write_io_errs, read_io_errs, flush_io_errs, corruption_errs, generation_errs. Any non-zero counter above acked baseline triggers alert.
- **Single alert tier**: the beep is just a "go check your NAS" signal. `braid status` is where the user learns what's actually wrong (errors vs missing device vs both).
- **Ack silences everything**: `braid ack` stops all beeping, including missing-device alerts. The monitor won't re-trigger for the same condition. If a *different* drive goes missing after ack, that's a new event and beeping resumes. Rationale: user heard the alert, they're working on it (ordering a drive, etc.) — beeping while they can't act is just annoying.
- **Ack state keyed by btrfs devid**: devid is btrfs-native — no need to cross-reference config or disk-map to identify missing devices. The `btrfs filesystem show` parser already sees missing device devids; we just need to stop discarding them.
- **Ack state is machine-local, not pool-portable**: on a new machine, `/var/lib/braid/acked-device-stats.json` doesn't exist, so everything evaluates fresh. This is correct — a new machine should assess drives independently.
- **Ack state separate from disk-map.json**: different concerns (identity vs operator acknowledgment), different write patterns (mutating commands vs monitor timer), different risk profiles (precious vs disposable).
- **`braid monitor` is a pure detector**: it checks state and returns exit 0 (ok) or exit 1 (alert needed). It does not start/stop services or write files. The systemd service wrapper starts the alert on exit 1, does nothing on exit 0.
- **Alerts are latched**: once triggered, alerts persist until `braid ack` — even if the triggering condition later disappears (e.g. transient error, drive replaced). A beep means "something happened that needs acknowledgment," not "something is currently true." This avoids the cross-source ownership bug where btrfs monitor exit 0 could accidentally clear a smartd-triggered alert. Matches Synology UX: beep until a human mutes it.
- **Periodic one-shot, not daemon**: systemd timer + oneshot service. No mount condition on the timer — `braid monitor` handles pool-not-mounted gracefully (exit 0).
- **Monitor self-heals ack state**: after a drive replacement, the monitor detects the previously-missing devid is now present and resets `missing_acked` to false. No coupling between replace/add commands and monitoring — the monitor owns its own state.
- **On by default**: `braid.monitor.enable` defaults to true when `braid.enable` is true. beep/pcspkr failures are silently swallowed, so it's harmless on hardware without a speaker.

## Implementation

### Step 1: Acked stats state management (Rust)

New file: `cli/src/acked_stats.rs`

Keyed by btrfs devid (string). devid is btrfs-native — no need to cross-reference config or disk-map to identify missing devices.

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

On-disk format (`/var/lib/braid/acked-device-stats.json`):

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
  },
  "2": {
    "missing_acked": true,
    "device_stats": {
      "read_io_errs": 0,
      "write_io_errs": 0,
      "flush_io_errs": 0,
      "corruption_errs": 0,
      "generation_errs": 0
    }
  }
}
```

- Follows `disk_map.rs` patterns: `load_acked_stats()` / `save_acked_stats()` using `atomic_write` from `state_io.rs`
- Returns empty struct on missing/corrupt file (same as `load_disk_map`)
- Comparison function: `check_for_new_errors(current_stats, acked, missing_devids) -> AlertCheck`
  - For each device: if any current counter > acked counter → new errors
  - Counter reset detection: if current < acked, treat acked as 0 for that device (remount reset the counters)
  - Missing device: if devid is missing and `missing_acked` is false → alert
  - Missing device already acked: no alert for that devid. But if a *different* devid goes missing → new alert
  - Returns: `AlertCheck { alert_needed: bool, devices_with_new_errors: Vec<u64>, new_missing: Vec<u64> }`
- `snapshot_current(current_stats, missing_devids) -> AckedStats` — captures current values + sets `missing_acked: true` for missing devids
- Register in `cli/src/lib.rs`
- Unit tests: roundtrip, comparison logic, counter reset detection, missing ack, empty file

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

`braid monitor` is a **pure detector** — it checks state, returns an exit code, and does nothing else. It does not start/stop services or write files.

`cmd_monitor(runner, fs, config) -> Result<MonitorResult>`

1. Check if pool is mounted (via `probe_pool`). If not → return `MonitorResult::PoolOffline` (exit 0).
2. Run `btrfs device stats` (existing `CmdRequest::BtrfsDeviceStats`).
3. Load acked stats.
4. Get missing devids (from pool probe's new `missing_devids` field).
5. **Self-heal stale ack state**: for any devid where `missing_acked: true` but the devid is now present, reset `missing_acked` to false. Save updated ack file if changed.
6. Call `check_for_new_errors()`.
7. Return `MonitorResult` with `alert_needed: bool`.

Exit codes:
- 0: no alert needed (or pool offline)
- 1: alert needed (new errors or missing device)

Wire in `cli/src/main.rs`:
- Add `Monitor` to `Commands` enum (no args)
- Map result to process exit code

Reuses:
- `CmdRequest::BtrfsDeviceStats` (`cli/src/cmd.rs`)
- `parse_btrfs_device_stats` (`cli/src/parse/btrfs_device_stats.rs`)
- `probe_pool` (`cli/src/probe.rs`)

### Step 4: `braid ack` command (Rust)

New file: `cli/src/ack.rs`

`cmd_ack(runner, config) -> Result<()>`

1. Check if pool is mounted. If not → error.
2. Run `btrfs device stats`.
3. Get missing devids (from pool probe).
4. `snapshot_current(stats, missing_devids)` → save to acked stats file (sets `missing_acked: true` for missing devids).
5. Stop alert service: `std::process::Command::new(systemctl_path).args(["stop", "braid-alert.service"])` — best-effort, warn on failure. Uses the systemd binary from the wrapper PATH (see Step 5 — systemd added to `toolPackages`).
6. Print confirmation: "acknowledged errors on N devices"

Wire in `cli/src/main.rs`:
- Add `Ack` to `Commands` enum (no args)

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
  ${pkgs.systemd}/bin/systemctl start braid-alert.service 2>/dev/null || true
'';
```

smartd handles its own state tracking (persists in `/var/lib/smartmontools/`). braid doesn't track SMART state — smartd fires the exec script only when something changes.

### Step 7: `braid status` enhancements

In `cli/src/status.rs`:

- When a disk has non-zero errors, add action guidance to human-readable output:
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

- Add `device_errors: HashMap<String, DiskErrors>` to `PoolState` in `cli/src/tui/model.rs`
- Probe device errors in `cli/src/tui/probe.rs` (reuse existing `CmdRequest::BtrfsDeviceStats`)
- Disk table in `cli/src/tui/view/mod.rs`: show error total per disk, red when non-zero
- Disk detail popup: show all five error counters
- Update snapshot tests with error data

### Step 9: NixOS VM tests

#### Test: monitor + ack workflow (`tests/cli/braid-monitor.py`)

Intent: Verify `braid monitor` detects missing devices and `braid ack` clears alert state.

Scenario:
1. Create 3-disk RAID1 pool
2. `braid monitor` → exit 0 (no errors)
3. Close one LUKS mapper, remount degraded → missing device
4. `braid monitor` → exit 1 (alert needed)
5. `braid ack` → verify acked state file written, contains `missing_acked: true` for the missing devid
6. `braid monitor` → exit 0 (missing device acknowledged, no re-trigger for same condition)

Note: Testing actual beep output is not feasible in VM. Tests verify service lifecycle and exit codes.

#### Test: alert service lifecycle (`tests/module/braid-alert.py`)

Intent: Verify systemd units exist and can be started/stopped.

1. Enable `braid.monitor`
2. Verify `braid-monitor.timer` is active
3. Verify `braid-alert.service` unit exists
4. `systemctl start braid-alert.service` → verify active → `systemctl stop` → verify inactive

### Step 10: Docs

- Update `README.md` with monitoring section
- Add `docs/decisions/disk-health-monitoring.md` (ADR, status: Active)

## Files to create

| File | Purpose |
|------|---------|
| `cli/src/acked_stats.rs` | Acked state management |
| `cli/src/monitor.rs` | `braid monitor` command |
| `cli/src/ack.rs` | `braid ack` command |
| `modules/braid/monitor.nix` | NixOS services/timer/smartd |
| `tests/cli/braid-monitor.py` | VM test: monitor + ack |
| `tests/cli/braid-monitor.nix` | VM test: NixOS config |
| `tests/module/braid-alert.py` | VM test: alert service |
| `tests/module/braid-alert.nix` | VM test: NixOS config |
| `docs/decisions/disk-health-monitoring.md` | ADR |

## Files to modify

| File | Change |
|------|--------|
| `cli/src/lib.rs` | Add `pub mod acked_stats;`, `pub mod monitor;`, `pub mod ack;` |
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

1. `just test-rust` — unit tests for acked_stats comparison logic
2. `just test braid-monitor` — VM test for monitor + ack CLI workflow
3. `just test braid-alert` — VM test for systemd service lifecycle
4. Manual: on a NixOS machine with `braid.monitor.enable = true`, verify `systemctl list-timers` shows `braid-monitor.timer`, and `braid monitor` exits 0 on a healthy pool
