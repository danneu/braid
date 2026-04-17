# Plan: `braid.fanControl` NixOS module

## Context

braid users who want HDD-temperature-driven chassis fan control currently have to
wire up hddfancontrol, drivetemp kernel modules, udev hotswap rules, and systemd
crash recovery themselves. This is ~100 lines of non-obvious NixOS config
(debounced oneshot for udev events, asymmetric add/remove rules because
SYSTEMD_WANTS doesn't fire on device departure, hddtemp force-disable, etc.).

braid already absorbs this kind of complexity for auto-suspend
(`braid.autoSuspend`). Fan control is the same pattern: the user provides one
hardware-specific value (PWM path from `pwmconfig`), and braid wires up everything
else with sane HDD defaults.

Target: SATA (ATA) drives only. SAS is out of scope (different transport, requires
HBA, tiny overlap with NixOS home NAS users).

**Scope boundary:** `braid.fanControl` monitors **all visible SATA devices** via
hddfancontrol's `ata` selector, not only braid pool members. Fan control is a
chassis safety loop -- drives generate heat regardless of LUKS state, pool
membership, or mount status. Binding fan control to pool state would leave warm
disks uncooled when the pool is locked or before first unlock.

---

## 1. Options API

```nix
braid.fanControl = {
  enable = true;
  pwmPath = "`echo /sys/devices/platform/f71882fg.656/hwmon/hwmon[[:print:]]`/device/pwm2";
  minStart = 65;  # from hddfancontrol pwm-test
  maxStop = 60;   # from hddfancontrol pwm-test
  # Everything below has sane defaults:
  # minTemp = 30;
  # maxTemp = 40;
  # minFanSpeedPercent = 20;
  # interval = "30s";
};
```

| Option | Type | Default | Description |
|---|---|---|---|
| `enable` | bool | `false` | Enable HDD temperature-driven fan control |
| `pwmPath` | string | `""` (required) | Sysfs path to chassis fan PWM control file |
| `minStart` | int 0-255 | (required) | Minimum PWM to start fan from standstill |
| `maxStop` | int 0-255 | (required) | PWM below which a spinning fan stalls |
| `minTemp` | int 0-100 | `30` | Temperature (C) below which the fan runs at minimum speed |
| `maxTemp` | int 0-100 | `40` | Temperature (C) above which the fan runs at full speed |
| `minFanSpeedPercent` | int 0-100 | `20` | Minimum fan speed %. Prevents fan from fully stopping |
| `interval` | string | `"30s"` | Polling interval (hddfancontrol duration format) |

`pwmPath`, `minStart`, and `maxStop` are all required. Run `pwmconfig` to find the
PWM path, then `hddfancontrol pwm-test -p <pwm-path>` to measure minStart/maxStop.
These are hardware calibration data specific to each fan -- there is no safe
universal default.

`minFanSpeedPercent` defaults to 20 (matches hddfancontrol's upstream conservative
default). Setting to 0 allows the fan to stop entirely below minTemp, which
hddfancontrol warns against unless the system has other cooling.

**Not exposed** (hardcoded):
- `drives` -- `[ "ata" ]` (auto-discover all SATA drives)
- `--restore-fan-settings` -- always on
- `logVerbosity` -- `INFO`

**Hardware prerequisites (user's responsibility, documented in manual):**
The PWM path exists only if the user has loaded their board's Super I/O kernel
driver (`nct6775`, `f71882fg`, `it87`, etc.) in `boot.kernelModules` and applied
any required kernel params (e.g. `acpi_enforce_resources=lax`). The module loads
`drivetemp` for drive temps but cannot detect or load the board-specific driver.
The fan control guide documents the full discovery workflow.

---

## 2. Files to create/modify

### New: `modules/braid/fan-control.nix`

Options and implementation in the same file (follows `auto-suspend.nix` pattern),
guarded by `lib.mkIf (cfg.enable && cfg.fanControl.enable)`.

```nix
# HDD-driven chassis fan control via hddfancontrol.
#
# Defines the systemd service directly instead of using the nixpkgs
# services.hddfancontrol module. The nixpkgs module unconditionally enables
# hddtemp (unnecessary with drivetemp) and injects hddtemp.service
# dependencies that must then be force-overridden. Owning the service avoids
# that brittleness and gives braid full control over the unit lifecycle.
{ config, lib, pkgs, ... }:
let
  cfg = config.braid;
  fc = cfg.fanControl;
  pwmSpec = "${fc.pwmPath}:${toString fc.minStart}:${toString fc.maxStop}";
in
{
  options.braid.fanControl = {
    enable = lib.mkEnableOption "HDD temperature-driven fan control";

    pwmPath = lib.mkOption {
      type = lib.types.str;
      default = "";
      example = "`echo /sys/devices/platform/f71882fg.656/hwmon/hwmon[[:print:]]`/device/pwm2";
      description = ''
        Sysfs path to the chassis fan PWM control file. Shell globs and
        backtick substitution are supported for hwmon numbering (e.g.
        `echo .../hwmon[[:print:]]`). Run `pwmconfig` to find this path.

        Requires a board-specific Super I/O kernel driver (e.g. nct6775,
        f71882fg, it87) loaded in boot.kernelModules. See the fan control
        guide for the full discovery workflow.
      '';
    };

    minStart = lib.mkOption {
      type = lib.types.ints.between 0 255;
      description = ''
        Minimum PWM value to start the fan from standstill. Run
        `hddfancontrol pwm-test -p <pwm-path>` to measure this for your fan.
      '';
    };

    maxStop = lib.mkOption {
      type = lib.types.ints.between 0 255;
      description = ''
        PWM value below which a spinning fan stalls. Run
        `hddfancontrol pwm-test -p <pwm-path>` to measure this for your fan.
      '';
    };

    minTemp = lib.mkOption {
      type = lib.types.ints.between 0 100;
      default = 30;
      description = "Temperature (Celsius) below which the fan runs at minimum speed.";
    };

    maxTemp = lib.mkOption {
      type = lib.types.ints.between 0 100;
      default = 40;
      description = "Temperature (Celsius) above which the fan runs at full speed.";
    };

    minFanSpeedPercent = lib.mkOption {
      type = lib.types.ints.between 0 100;
      default = 20;
      description = ''
        Minimum fan speed as percentage of range. 20 is hddfancontrol's
        upstream conservative default. Setting to 0 allows the fan to stop
        entirely below minTemp -- only safe if the system has other cooling.
      '';
    };

    interval = lib.mkOption {
      type = lib.types.str;
      default = "30s";
      description = "Temperature polling interval (e.g. '30s', '1min').";
    };
  };

  config = lib.mkIf (cfg.enable && fc.enable) {
    assertions = [
      {
        assertion = fc.pwmPath != "";
        message = ''
          braid.fanControl.pwmPath is required. This is the sysfs path to the
          chassis fan's PWM control file (e.g. /sys/devices/platform/.../pwmN).
          Run `pwmconfig` to discover this path. See the fan control guide for
          the full hardware discovery workflow.
        '';
      }
      {
        assertion = fc.maxStop <= fc.minStart;
        message = "braid.fanControl.maxStop (${toString fc.maxStop}) must be <= "
          + "minStart (${toString fc.minStart}). maxStop is the PWM below which a "
          + "spinning fan stalls; minStart is the PWM needed to start from standstill. "
          + "Run `hddfancontrol pwm-test -p <pwm-path>` to measure these values.";
      }
      {
        assertion = fc.minTemp < fc.maxTemp;
        message = "braid.fanControl.minTemp (${toString fc.minTemp}) "
          + "must be less than maxTemp (${toString fc.maxTemp}).";
      }
    ];

    # Expose SATA drive SMART temperatures as hwmon inputs. drivetemp reads
    # via the ATA SCT command, which does not wake sleeping drives (unlike
    # hddtemp's SCSI INQUIRY approach).
    boot.kernelModules = [ "drivetemp" ];

    # --- hddfancontrol daemon ---
    #
    # Defined directly rather than via the nixpkgs services.hddfancontrol
    # module. No hddtemp daemon dependency: hddfancontrol tries drivetemp
    # first in its probe chain (src/probe/mod.rs:84 in 2.0.6), and drivetemp
    # is loaded via boot.kernelModules above.
    #
    # disks = "ata" monitors ALL visible SATA devices, not only braid pool
    # members. Fan control is a chassis safety loop -- drives generate heat
    # regardless of LUKS/btrfs state.
    systemd.services.hddfancontrol-braid = {
      description = "HDD fan control (braid)";
      wantedBy = [ "multi-user.target" ];
      serviceConfig = {
        # Security hardening (matches nixpkgs hddfancontrol module).
        CPUSchedulingPolicy = "rr";
        CPUSchedulingPriority = 49;
        ProtectSystem = "strict";
        PrivateTmp = true;
        ProtectHome = true;
        SystemCallArchitectures = "native";
        MemoryDenyWriteExecute = true;
        NoNewPrivileges = true;
        # Crash recovery: restart on mid-probe drive removal or transient
        # hwmon read errors during hot-swap events.
        Restart = "always";
        RestartSec = 5;
      };
      script = ''
        exec ${lib.getExe pkgs.hddfancontrol} -v INFO daemon \
          -d ata \
          -p ${pwmSpec} \
          --drive-temp-range ${toString fc.minTemp} ${toString fc.maxTemp} \
          --min-fan-speed-prct ${toString fc.minFanSpeedPercent} \
          --interval ${fc.interval} \
          --restore-fan-settings
      '';
    };

    # --- SATA hotswap support ---
    #
    # hddfancontrol resolves drives once at startup and holds that list for
    # the process lifetime. Adding a drive leaves it unmonitored; removing
    # one crashes the daemon on the next probe cycle. The udev rules below
    # restart the daemon on topology changes so the ata selector re-resolves.

    systemd.services.braid-fan-reload = {
      description = "Restart hddfancontrol after SATA drive topology change";
      serviceConfig = {
        Type = "oneshot";
        # Debounce: SATA hotplug produces multiple udev events in quick
        # succession. While this oneshot is active (in the sleep), further
        # start requests are no-ops -- events collapse into one restart.
        ExecStartPre = "${pkgs.coreutils}/bin/sleep 5";
        ExecStart = "${pkgs.systemd}/bin/systemctl restart hddfancontrol-braid.service";
      };
    };

    # Two rules because the systemd/udev integration is asymmetric:
    # - SYSTEMD_WANTS fires on device add (documented device-unit activation)
    # - SYSTEMD_WANTS does NOT fire on device remove; RUN+= is needed instead
    # ID_BUS=="ata" filters out USB mass storage (also appears as /dev/sd*).
    # On remove, ID_BUS persists in the udev database from the earlier add
    # event -- pragmatically reliable on current systemd, with Restart=always
    # as fallback if it ever doesn't match.
    services.udev.extraRules = ''
      ACTION=="add", SUBSYSTEM=="block", KERNEL=="sd*", ENV{DEVTYPE}=="disk", ENV{ID_BUS}=="ata", TAG+="systemd", ENV{SYSTEMD_WANTS}+="braid-fan-reload.service"
      ACTION=="remove", SUBSYSTEM=="block", KERNEL=="sd*", ENV{DEVTYPE}=="disk", ENV{ID_BUS}=="ata", RUN+="${pkgs.systemd}/bin/systemctl start --no-block braid-fan-reload.service"
    '';
  };
}
```

### Modify: `modules/braid/default.nix`

Add `./fan-control.nix` to the imports:

```nix
{ imports = [ ./options.nix ./storage.nix ./cli.nix ./monitor.nix ./auto-suspend.nix ./fan-control.nix ]; }
```

### No changes to `flake.nix` (for the module)

hddfancontrol is not a parser-critical tool (braid never parses its output), so it
doesn't need pinning under `braid.packages.*`. The binary comes from
`pkgs.hddfancontrol` (available in nixpkgs). The nixpkgs `services.hddfancontrol`
module is NOT used -- braid defines its own systemd service directly. `flake.nix`
is updated only to register the new VM tests (see section 3).

---

## 3. VM tests

Two tests: a wiring test that inspects generated units and rules, and a hotswap
test that verifies udev-triggered service restarts with a real SATA device.

### Test 1: `tests/module/fan-control.nix` + `.py` (wiring)

Validates NixOS wiring: evaluation, generated service script, restart policy, udev
rules, drivetemp, and no hddtemp daemon dependency. No real hwmon/PWM devices -- inspects
generated artifacts only.

```nix
# Test: fan-control
#
# Intent: Verify that braid.fanControl generates the correct systemd service,
# hddfancontrol arguments, restart policy, and udev rules.
#
# Why it exists: The module defines a systemd service directly, loads drivetemp,
# generates udev hotswap rules, and creates a debounced restart oneshot. Any
# misconfiguration is silent until a user reports broken fan control on real
# hardware. This test catches wiring regressions at generation time.
#
# Scenario: NixOS VM with braid.fanControl enabled and a fake PWM path. No real
# hwmon devices -- inspects generated unit files and udev rules, not runtime
# behavior.
{ braid }:
{
  name = "fan-control";

  nodes.machine = { ... }: {
    imports = [ ../../modules/braid ];

    braid = {
      enable = true;
      package = braid;

      fanControl = {
        enable = true;
        pwmPath = "/sys/class/hwmon/hwmon0/pwm1";
        minStart = 65;
        maxStop = 60;
        minTemp = 25;
        maxTemp = 45;
        minFanSpeedPercent = 10;
        interval = "20s";
      };
    };
  };

  testScript = builtins.readFile ./fan-control.py;
}
```

```python
# Test: fan-control (wiring)

start_all()
machine.wait_for_unit("multi-user.target")

with subtest("drivetemp kernel module is loaded"):
    machine.succeed("lsmod | grep -q drivetemp")

with subtest("hddfancontrol-braid service has correct arguments"):
    # NixOS generates a wrapper script for `script =` directives. Extract
    # the script path from ExecStart and read it.
    exec_start = machine.succeed(
        "systemctl show hddfancontrol-braid.service -p ExecStart --value"
    ).strip()
    script_path = exec_start.split("path=")[1].split(";")[0].strip()
    script = machine.succeed(f"cat {script_path}")
    assert "-d ata" in script, f"Expected '-d ata' in script:\n{script}"
    assert "/sys/class/hwmon/hwmon0/pwm1:65:60" in script, \
        f"Expected PWM path:minStart:maxStop in script:\n{script}"
    assert "--drive-temp-range 25 45" in script, f"Expected temp range in script:\n{script}"
    assert "--min-fan-speed-prct 10" in script, f"Expected min fan speed in script:\n{script}"
    assert "--interval 20s" in script, f"Expected interval in script:\n{script}"
    assert "--restore-fan-settings" in script, f"Expected --restore-fan-settings in script:\n{script}"

with subtest("hddfancontrol-braid has Restart=always and RestartSec=5s"):
    restart = machine.succeed(
        "systemctl show hddfancontrol-braid.service -p Restart --value"
    ).strip()
    assert restart == "always", f"Expected Restart=always, got {restart}"
    restart_sec = machine.succeed(
        "systemctl show hddfancontrol-braid.service -p RestartUSec --value"
    ).strip()
    assert restart_sec == "5s", f"Expected RestartUSec=5s, got {restart_sec}"

with subtest("no hddtemp daemon dependency"):
    after = machine.succeed(
        "systemctl show hddfancontrol-braid.service -p After --value"
    ).strip()
    assert "hddtemp" not in after, f"hddtemp found in After: {after}"
    machine.fail("systemctl cat hddtemp.service")

with subtest("braid-fan-reload oneshot exists with debounce"):
    unit = machine.succeed("systemctl cat braid-fan-reload.service")
    assert "restart hddfancontrol-braid.service" in unit
    assert "sleep 5" in unit

with subtest("udev rules have correct add and remove SATA hotswap rules"):
    rules = machine.succeed(
        "grep -r 'braid-fan-reload' /etc/udev/rules.d/"
    ).strip()
    assert 'ACTION=="add"' in rules
    assert 'ENV{SYSTEMD_WANTS}+="braid-fan-reload.service"' in rules
    assert 'ACTION=="remove"' in rules
    assert "systemctl start --no-block braid-fan-reload.service" in rules
    assert rules.count('ENV{ID_BUS}=="ata"') >= 2, \
        "Expected ID_BUS filter on both add and remove rules"

machine.shutdown()
```

### Test 2: `tests/module/fan-control-hotswap.nix` + `.py` (hotswap behavior)

Validates the hotswap restart path end-to-end: attaches a SATA disk via QEMU AHCI,
triggers a udev add event, and verifies that `braid-fan-reload` fires and restarts
`hddfancontrol-braid`. The daemon is overridden with a test stub (`sleep infinity`)
since there are no real hwmon/PWM devices in the VM.

```nix
# Test: fan-control-hotswap
#
# Intent: Verify that SATA hotplug events trigger braid-fan-reload, which
# restarts hddfancontrol-braid. Tests the udev rule -> oneshot -> daemon
# restart chain end-to-end.
#
# Why it exists: The udev hotswap rules are the most failure-prone part of
# fan control. The add rule uses SYSTEMD_WANTS, the remove rule uses RUN+=,
# and both depend on ID_BUS=="ata" being set. A wiring-only test (grep rules
# files) cannot verify that these rules actually fire on real block device
# events.
#
# Scenario: QEMU VM with an AHCI (SATA) controller and one attached disk.
# The daemon is stubbed with `sleep infinity` so it can start without hwmon.
# The test triggers a synthetic udev add event on the SATA disk and verifies
# the daemon restarts by observing its ActiveEnterTimestamp change.
{ braid }:
{
  name = "fan-control-hotswap";

  nodes.machine = { pkgs, lib, ... }: {
    imports = [ ../../modules/braid ];

    braid = {
      enable = true;
      package = braid;

      fanControl = {
        enable = true;
        pwmPath = "/sys/class/hwmon/hwmon0/pwm1";
        minStart = 65;
        maxStop = 60;
      };
    };

    # Attach a disk via AHCI so it appears as /dev/sdX with ID_BUS=ata.
    virtualisation.qemu.options = [
      "-device" "ich9-ahci,id=ahci0"
      "-device" "ide-hd,drive=sata0,bus=ahci0.0,serial=sata-test"
    ];
    virtualisation.emptyDiskImages = [
      { size = 64; driveConfig.deviceExtraOpts.id = "sata0"; driveConfig.deviceExtraOpts.if = "none"; }
    ];

    # Override the daemon with a test stub -- no real hwmon in the VM.
    systemd.services.hddfancontrol-braid.script = lib.mkForce ''
      exec ${pkgs.coreutils}/bin/sleep infinity
    '';
  };

  testScript = builtins.readFile ./fan-control-hotswap.py;
}
```

```python
# Test: fan-control-hotswap

start_all()
machine.wait_for_unit("multi-user.target")

with subtest("SATA disk is present with ID_BUS=ata"):
    # Verify the AHCI disk appeared and has the right udev properties.
    machine.succeed("udevadm info --query=property /dev/sda | grep -q 'ID_BUS=ata'")

with subtest("hddfancontrol-braid is running (stub)"):
    machine.succeed("systemctl is-active hddfancontrol-braid.service")

with subtest("udev add event triggers braid-fan-reload and restarts daemon"):
    # Record the current start timestamp.
    ts_before = machine.succeed(
        "systemctl show hddfancontrol-braid.service -p ActiveEnterTimestamp --value"
    ).strip()
    # Fire a synthetic add event on the SATA disk.
    machine.succeed("udevadm trigger --action=add --subsystem-match=block /sys/block/sda")
    # Wait for the debounced restart: 5s sleep + restart time.
    machine.sleep(8)
    ts_after = machine.succeed(
        "systemctl show hddfancontrol-braid.service -p ActiveEnterTimestamp --value"
    ).strip()
    assert ts_before != ts_after, \
        f"Daemon was not restarted by add event. Before: {ts_before}, after: {ts_after}"

with subtest("udev remove event triggers braid-fan-reload and restarts daemon"):
    ts_before = machine.succeed(
        "systemctl show hddfancontrol-braid.service -p ActiveEnterTimestamp --value"
    ).strip()
    machine.succeed("udevadm trigger --action=remove --subsystem-match=block /sys/block/sda")
    machine.sleep(8)
    ts_after = machine.succeed(
        "systemctl show hddfancontrol-braid.service -p ActiveEnterTimestamp --value"
    ).strip()
    assert ts_before != ts_after, \
        f"Daemon was not restarted by remove event. Before: {ts_before}, after: {ts_after}"

machine.shutdown()
```

Note: The hotswap test's QEMU AHCI configuration may need adjustment during
implementation. The key requirement is that the disk appears as `/dev/sdX` with
`ID_BUS=ata` in the udev database. If `virtualisation.emptyDiskImages` doesn't
support the `id`/`if` drive options directly, use `virtualisation.qemu.options` to
pass raw `-drive` flags instead.

### Modify: `flake.nix`

Register both tests:

```nix
fan-control = pkgs.testers.nixosTest (
  import ./tests/module/fan-control.nix {
    braid = linuxCrane.braid-cli-unwrapped;
  }
);
fan-control-hotswap = pkgs.testers.nixosTest (
  import ./tests/module/fan-control-hotswap.nix {
    braid = linuxCrane.braid-cli-unwrapped;
  }
);
```

---

## 4. Manual updates

### Modify: `manual/guides/fan-control.md`

The guide currently says "braid itself does not manage fans" (line 15) and
recommends `hardware.fancontrol`/`fancontrol` as the primary recipe. Update to
make `braid.fanControl` the recommended approach:

- **Replace the intro** (lines 5-15): Remove "braid itself does not manage fans."
  State that braid provides `braid.fanControl` which handles hddfancontrol, SATA
  hotswap, and crash recovery. The user provides one hardware-specific value
  (PWM path from `pwmconfig`).

- **Replace "The stack" section** (lines 17-28): Update to describe the new stack
  (drivetemp + Super I/O driver + hddfancontrol + braid module), dropping the
  fancontrol-centric framing.

- **Rewrite the Discovery section** (lines 30-131) for hddfancontrol:
  - Keep "Install the tooling and load the sensor modules" -- still required.
  - Keep "Find your Super I/O chip" -- still required.
  - Keep "'Device or resource busy' on module load" -- still relevant.
  - Keep "Map fans to PWMs with pwmconfig" -- still required to find the PWM path,
    MINSTART, and MINSTOP values.
  - Keep "Tuning MINSTART and MINSTOP" -- still relevant.
  - **Drop "Identifying the pilot drive"** (lines 117-131): This was a workaround
    for vanilla fancontrol's one-input-per-PWM limitation. hddfancontrol reads all
    SATA drives and ramps from the hottest -- no pilot selection needed.
  - Remove all references to `fancontrol` as the target daemon; the discovery
    output now feeds into `braid.fanControl.pwm`.

- **Replace "Committing to Nix" section** (lines 133-187): Replace the
  `hardware.fancontrol.config` recipe with the `braid.fanControl` recipe:
  ```nix
  braid.fanControl = {
    enable = true;
    pwmPath = "`echo /sys/devices/platform/.../hwmon/hwmon[[:print:]]`/device/pwmN";
    minStart = 65;  # from hddfancontrol pwm-test
    maxStop = 60;   # from hddfancontrol pwm-test
  };
  ```

- **Drop "After a drive swap or re-cable" section** (lines 212-219): The SATA
  hotswap udev rules handle topology changes automatically.

- **Replace "When vanilla fancontrol isn't enough" section** (lines 259-266): This
  section recommended hddfancontrol as an upgrade. Since braid now uses
  hddfancontrol, reframe as: "If you need control beyond what braid.fanControl
  provides (PID curves, multiple fans with different curves, etc.), configure
  `services.hddfancontrol` directly or use fan2go/CoolerControl."

- **Rewrite the worked example** (lines 268-343): Update the "Final modules and
  kernel params" and end-to-end check to show `braid.fanControl` instead of
  `hardware.fancontrol.config`. Keep the hardware discovery narrative (unknown chip,
  ACPI busy, pwmconfig mapping) -- that's still valuable.

- **Add explicit scope note**: State clearly that `braid.fanControl` monitors all
  visible SATA devices, not only braid pool members, because drives generate heat
  regardless of LUKS or btrfs state.

### Modify: `manual/guides/nixos-configuration.md`

Add a "Fan control" option table after the "Auto-suspend" section (after line 138):

```markdown
### Fan control

| Option | Type | Default | Description |
| --- | --- | --- | --- |
| `braid.fanControl.enable` | bool | `false` | Drive chassis fans from HDD temps |
| `braid.fanControl.pwmPath` | string | (required) | Sysfs path to chassis fan PWM file |
| `braid.fanControl.minStart` | int | (required) | Minimum PWM to start fan from standstill |
| `braid.fanControl.maxStop` | int | (required) | PWM below which a spinning fan stalls |
| `braid.fanControl.minTemp` | int | `30` | Temperature (C) at which fan runs at minimum speed |
| `braid.fanControl.maxTemp` | int | `40` | Temperature (C) at which fan runs at full speed |
| `braid.fanControl.minFanSpeedPercent` | int | `20` | Minimum fan speed % (0 = fan may stop) |
| `braid.fanControl.interval` | string | `"30s"` | Temperature polling interval |

`pwmPath` is found via `pwmconfig`. `minStart` and `maxStop` are measured with
`hddfancontrol pwm-test -p <pwm-path>`. All three are hardware-specific.

Monitors all visible SATA devices (not only braid pool members). Requires a
board-specific Super I/O driver in `boot.kernelModules` -- see
[Fan control](fan-control.md) for the hardware discovery workflow.
```

Add to the "What you get for free" section (around line 51):
```markdown
- **Fan control** (opt-in) -- drive chassis fans from the hottest SATA drive temp.
  Handles hddfancontrol, SATA hotswap restart, crash recovery. Configurable via `braid.fanControl`.
```

Add `fanControl` block to the full config example (around line 179):
```nix
  fanControl = {
    enable = false;    # default; opt-in
    pwmPath = "`echo /sys/devices/platform/.../hwmon/hwmon[[:print:]]`/device/pwmN";
    minStart = 65;     # from hddfancontrol pwm-test (required)
    maxStop = 60;      # from hddfancontrol pwm-test (required)
    minTemp = 30;      # default
    maxTemp = 40;      # default
    minFanSpeedPercent = 20;  # default
    interval = "30s";  # default
  };
```

Add to the "Related" section:
```markdown
- [Fan control](fan-control.md) -- hardware discovery and fan control setup
```

### Modify: `manual/index.md`

Add fan control to the guides table (line 22, after Power management):

```markdown
| [Fan control](guides/fan-control.md)                         | HDD-driven chassis fan control, SATA hotswap            |
```

### Note: `manual/book/`

`manual/book/` is checked-in mdBook build output. It needs rebuilding after the
markdown changes. Run `mdbook build` (or whatever build command the repo uses) and
commit the output.

---

## 5. Design decisions

**Own the service directly, not via nixpkgs module.** The nixpkgs
`services.hddfancontrol` module unconditionally enables hddtemp and injects
`hddtemp.service` dependencies. Wrapping it requires `lib.mkForce` overrides that
are brittle against upstream changes. Defining `systemd.services.hddfancontrol-braid`
directly is simpler: no hddtemp daemon to disable, no dependencies to clear, full
control over the unit lifecycle. hddfancontrol tries drivetemp first in its probe
chain (`src/probe/mod.rs:84` in 2.0.6), and the module loads drivetemp via
`boot.kernelModules`, so no hddtemp daemon dependency is needed.

**Not bound to pool lifecycle.** Drives generate heat regardless of LUKS state.
`hddfancontrol-braid` starts with `multi-user.target`, not `braid-online.service`.

**All SATA devices, not pool members.** Fan control is a chassis safety loop.
Scoping to pool members would leave non-pool SATA drives uncooled, create a
runtime dependency on pool.json, and fail on first boot before pool creation.

**Required fan calibration values.** `minStart`, `maxStop`, and `pwmPath` are all
required with no defaults. These are hardware-specific measurements -- there is no
safe universal default. `minFanSpeedPercent` defaults to 20 (upstream conservative
default) rather than 0, to avoid silently leaving the chassis uncooled.

**`KERNEL=="sd*"` + `ENV{DEVTYPE}=="disk"`** instead of `KERNEL=="sd[a-z]"`.
`DEVTYPE` is the canonical way to exclude partitions, and `sd*` handles 27+ block
devices (sdaa+).

**`ENV{ID_BUS}=="ata"`** filters USB mass storage. On remove events, `ID_BUS`
persists in the udev database from the add event -- pragmatically reliable, with
`Restart=always` as fallback.

---

## 6. Implementation order

1. Create `modules/braid/fan-control.nix`
2. Update `modules/braid/default.nix` (add import)
3. Create `tests/module/fan-control.nix` and `.py` (wiring test)
4. Create `tests/module/fan-control-hotswap.nix` and `.py` (hotswap test)
5. Register both tests in `flake.nix`
6. Run `just test-vm fan-control fan-control-hotswap` -- confirm both pass
7. Update `manual/guides/fan-control.md`
8. Update `manual/guides/nixos-configuration.md`
9. Update `manual/index.md`
10. Rebuild `manual/book/`

---

## 7. Verification

**Automated (VM tests):**
- `just test-vm fan-control` -- validates NixOS wiring
- `just test-vm fan-control-hotswap` -- validates udev-triggered restart chain

**Wiring test covers:**
- drivetemp kernel module loaded
- `hddfancontrol-braid.service` exists with correct `-d ata`, `-p` (assembled
  `pwmPath:minStart:maxStop`), temp range, interval, min speed,
  `--restore-fan-settings` args
- `Restart=always`, `RestartSec=5s` on the service
- No hddtemp daemon dependency
- `braid-fan-reload.service` oneshot with debounce sleep and restart command
- udev rules with correct add/remove structure and `ID_BUS` filter

**Hotswap test covers:**
- SATA disk present with `ID_BUS=ata`
- Daemon running (stub)
- Synthetic udev add event triggers `braid-fan-reload` and restarts daemon
  (verified via `ActiveEnterTimestamp` change)
- Synthetic udev remove event triggers the same restart chain via the
  `RUN+=systemctl` path (verified via `ActiveEnterTimestamp` change)

**Manual (real hardware only):**
- Confirm PWM writes work: `journalctl -u hddfancontrol-braid -f` during a scrub
- Confirm crash recovery: `systemctl kill -s KILL hddfancontrol-braid`, verify
  restart after 5s

---

## Critical files

| File | Action |
|---|---|
| `modules/braid/fan-control.nix` | **Create** -- full module |
| `modules/braid/default.nix` | **Edit** -- add import |
| `tests/module/fan-control.nix` | **Create** -- wiring test config |
| `tests/module/fan-control.py` | **Create** -- wiring test script |
| `tests/module/fan-control-hotswap.nix` | **Create** -- hotswap test config |
| `tests/module/fan-control-hotswap.py` | **Create** -- hotswap test script |
| `flake.nix` | **Edit** -- register both tests |
| `manual/guides/fan-control.md` | **Edit** -- rewrite for braid.fanControl |
| `manual/guides/nixos-configuration.md` | **Edit** -- add option table |
| `manual/index.md` | **Edit** -- add to guides table |
| `manual/book/` | **Rebuild** -- regenerate from markdown |
