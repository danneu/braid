# Add `braid.monitor.beep` option

## Context

The alert service in `monitor.nix` always beeps on disk errors. Users with headless boxes or who alert via other means (ntfy, email via `alertCommand`) may want to disable the audible beep.

## Execution Order (TDD)

1. Write `braid-alert-no-beep` test + register in `flake.nix`
2. Run `just test braid-alert-no-beep` — confirm it fails (missing `beep` option)
3. Implement module changes in `monitor.nix`
4. Run `just test braid-alert-no-beep` — confirm it passes
5. Run `just test braid-alert` — confirm existing test still passes
6. Update README

## Changes

### 1. New test: `tests/module/braid-alert-no-beep.nix` + `.py` (write first)

Register in `flake.nix` (~line 406, after existing `braid-alert` check).

**`braid-alert-no-beep.nix`**: same structure as `braid-alert.nix` but with:
```nix
braid.monitor.beep = false;
braid.monitor.alertCommand = "touch /root/alert-fired";
```

**`braid-alert-no-beep.py`** subtests:
- Monitor timer is active
- Alert service unit exists
- Service script does NOT contain modprobe/pcspkr/setpriv/beep references
- alertCommand still runs as root (start service → check /root/alert-fired → verify root ownership)
- Service stays active after script exits (RemainAfterExit — `systemctl is-active` succeeds)
- Service can be stopped (simulating braid ack)
- pcspkr not in boot modules (`! grep pcspkr /etc/modules-load.d/*`)
- pcspkr blacklist NOT removed from modprobe config (`grep pcspkr /etc/modprobe.d/*.conf` — the blacklist entry must still be present)
- beep group absent (`! getent group beep`)

### 2. `modules/braid/monitor.nix`

**Add local** at top of `let` block:

```nix
beepEnabled = cfg.monitor.beep;
```

**Add option** (next to existing `alertCommand`, ~line 22):

```nix
beep = lib.mkOption {
  type = lib.types.bool;
  default = true;
  description = "Emit an audible beep via the PC speaker on disk health alerts.";
};
```

**Guard PC speaker infrastructure** — wrap the overlay, `boot.kernelModules`, `users.groups.beep`, and `services.udev.extraRules` blocks (lines 32-50) with `lib.mkIf beepEnabled`.

**Use latched oneshot when beep is off** — make the service type conditional:

```nix
systemd.services.braid-alert = {
  description = "Braid disk health alert (audible beep if enabled)";
  serviceConfig = if beepEnabled then {
    Type = "simple";
  } else {
    Type = "oneshot";
    RemainAfterExit = true;
  };
  script = ''
    ${lib.optionalString beepEnabled ''
      ${pkgs.kmod}/bin/modprobe pcspkr 2>/dev/null || true
    ''}
    ${lib.optionalString (cfg.monitor.alertCommand != null) ''
      ${cfg.monitor.alertCommand} || true
    ''}
    ${lib.optionalString beepEnabled ''
      while true; do
        ${pkgs.util-linux}/bin/setpriv --reuid=nobody --regid=beep --groups=beep -- ${pkgs.beep}/bin/beep -f 1000 -l 500 2>/dev/null || true
        sleep 15
      done
    ''}
  '';
};
```

When `beep = false`: oneshot + `RemainAfterExit=true` — runs alertCommand (if set), exits, stays "active (exited)". `braid ack` → `systemctl stop` transitions to inactive. No sleeping process.

When `beep = true`: unchanged from current behavior (simple + infinite beep loop).

### 3. `README.md` (~lines 433-457)

- Line 433: soften "beeps until you acknowledge it" → "detects it and alerts you (audible beep by default)"
- Line 438: add "(if beep enabled)" after "starts an audible beeper"
- Line 442: make PC speaker paragraph conditional: "When `beep = true` (the default), braid automatically un-blacklists `pcspkr`..."
- Config example: add `beep = true;` line with comment `# audible PC speaker alert (disable for headless)`

## Verification

- `just test braid-alert` — existing test validates default (`beep = true`) path
- `just test braid-alert-no-beep` — new test validates `beep = false` path
- `just test braid-monitor` — alert lifecycle still works end-to-end
