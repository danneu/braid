# Plan: Monitor Lifecycle Integration Tests

## Context

Existing monitoring tests cover isolated pieces but never the full systemd chain:
- `tests/cli/braid-monitor.py` — CLI alert model (`braid monitor` exit codes, `braid status` banners, `braid ack`) but never exercises `braid-monitor.service` starting `braid-alert.service`
- `tests/module/braid-alert.py` / `braid-alert-no-beep.py` — alert unit plumbing (start/stop, beep config) but not the detector-to-alert wiring
- `tests/cli/braid-smartd-alert.py` — simulates the flag file but never invokes the actual smartd exec hook script
- `tests/module/smartd-config.py` — inspects rendered smartd.conf but never runs the hook
- Nothing verifies the documented `BindsTo=mnt-storage.mount` contract on `braid-monitor.service`

Two focused tests fill these gaps with minimal state churn per VM.

---

## Test 1: `monitor-lifecycle`

Focused on the mounted-pool monitor → alert → ack chain and mount-bound behavior.

### `tests/module/monitor-lifecycle.nix`

NixOS test config following the `systemd-lifecycle.nix` pattern:
- 3-disk RAID1 via `initrd-fixture.nix` (`diskNames = ["disk1" "disk2" "disk3"]`)
- `braid.monitor.beep = false` — alert service becomes oneshot+RAE (avoids infinite beep loop)
- `braid.monitor.alertCommand = "touch /root/alert-fired"` — observable side effect
- Seed `pool.json` with all 3 disks via `systemd.tmpfiles.rules`
- Override `braid-unlock.service` script with `lib.mkForce` (no interactive prompt)
- `braid = linuxCrane.braid-cli-unwrapped` (module wraps it)

### `tests/module/monitor-lifecycle.py`

| # | Subtest | What it proves |
|---|---------|---------------|
| 1 | Timer active at boot | `wantedBy = timers.target` works |
| 2 | No alert side effects before mount | Run `systemctl start braid-monitor.service`, then check: alert service not active, `/root/alert-fired` absent. Don't assert whether the start itself succeeds or fails — just that no alert-triggering side effects occur. Log the start result for debugging if useful. |
| 3 | Unlock pool via braid-pool.target | Precondition for remaining subtests |
| 4 | Healthy monitor run → no alert | `systemctl start braid-monitor.service` with healthy pool → alert service NOT active, alert-fired absent |
| 5 | Degrade pool | umount, `cryptsetup close braid-disk3`, `mount -o degraded`, wait for `mnt-storage.mount` active |
| 6 | Monitor triggers alert | `systemctl start braid-monitor.service` → `braid-alert.service` active + `/root/alert-fired` exists |
| 7 | Ack clears alert via systemd | `braid ack` → alert service stopped, `alert-latch.json` removed |
| 8 | No alert side effects after unmount | umount + LUKS close, run `systemctl start braid-monitor.service`, check: no alert service, no alert-fired. Same observable-behavior approach as subtest 2. |

Key implementation details:
- Subtests 2 and 8 assert **observable outcomes** (no alert fired, no alert service active), not the start command's exit code. This avoids coupling to systemd's exact BindsTo start-failure semantics and lets the test discover the real behavior.
- After manual `mount -o degraded` in subtest 5, `machine.wait_until_succeeds("systemctl is-active mnt-storage.mount")` guards against systemd mount auto-detection delay.
- Manual umount/close in subtest 8 (not `braid lock`) because disk3 mapper is already closed from subtest 5.

---

## Test 2: `smartd-hook`

Focused on the smartd exec bridge: script contents, invocation, and ack cleanup.

### `tests/module/smartd-hook.nix`

Smallest possible node that renders the smartd config and hook script:
- Import `../../modules/braid` with `braid.enable = true`, `monitor.enable = true`, `monitor.beep = false`
- `monitor.alertCommand = "touch /root/alert-fired"`
- No initrd-fixture, no storage disks. If the module requires disks to evaluate, add the minimum needed during implementation (2x 256MB), but try without first.
- `braid = linuxCrane.braid-cli-unwrapped`

### `tests/module/smartd-hook.py`

| # | Subtest | What it proves |
|---|---------|---------------|
| 1 | Hook script contents | Extract hook path from smartd.conf → verify it touches `/var/lib/braid/smartd-alert` and starts `braid-alert.service` |
| 2 | Hook invocation | Run the script → flag file created + alert service active + alertCommand fired |
| 3 | Ack clears smartd alert | `braid ack` → flag file removed + alert service stopped |

Key implementation details:
- Extract hook path from `systemctl show smartd.service -p ExecStart --value` → parse `--configfile=` → read config → parse `-M exec <path>`.
- `braid ack` in the offline path succeeds when `smartd_active` is true (`ack.rs:72`).
- `braid-alert.service` has no BindsTo on the mount, so it starts fine without the pool.

---

## Modified File

### `flake.nix`

Add two registrations near line 431:
```nix
monitor-lifecycle = pkgs.testers.nixosTest (
  import ./tests/module/monitor-lifecycle.nix {
    braid = linuxCrane.braid-cli-unwrapped;
  }
);
smartd-hook = pkgs.testers.nixosTest (
  import ./tests/module/smartd-hook.nix {
    braid = linuxCrane.braid-cli-unwrapped;
  }
);
```

## Verification

```
just test monitor-lifecycle smartd-hook
```

If either fails, run with `-v` on just that test:
```
just test monitor-lifecycle -v
```
