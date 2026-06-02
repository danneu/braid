[← braid](../index.md)

# Fan control

This guide covers how to drive chassis fans from HDD temperatures on a NixOS NAS using the `braid.fanControl` module.

Read this if you want quieter idle and predictable ramp under sustained disk load -- BIOS fan curves cannot see HDD temperatures, only CPU and motherboard temperatures.

## Why HDD-driven fan control

HDD longevity drops as drives run hotter, so the goal is to keep them under a target temperature -- a widely used rule of thumb is ~40 C. The catch is that the BIOS fan curve can't see drive temperature; it reads only CPU package temp and a motherboard sensor. So no matter how the BIOS ramps the chassis fans, nothing in that loop is actually watching the drives. The BIOS already protects the CPU regardless of its TDP -- the drives are the part left unmonitored.

The fix is to move fan control into Linux userspace using drive temps as the signal. The kernel's `drivetemp` module exposes each SATA drive's SMART temperature as a standard hwmon input, and `hddfancontrol` reads those inputs and drives the chassis fan's PWM proportionally to the hottest drive.

`braid.fanControl` wraps `hddfancontrol` so you only provide two hardware-specific values (the Super I/O platform device name and PWM channel number from `pwmconfig`) plus two calibration values (`pwm.minStart`/`pwm.maxStop` from `hddfancontrol pwm-test`). The module handles the systemd service, `drivetemp` loading, SATA hotswap udev rules, and crash recovery.

**Scope:** `braid.fanControl` monitors **all visible SATA devices**, not only braid pool members. Drives generate heat regardless of LUKS state, pool membership, or mount status -- binding fan control to pool state would leave warm disks uncooled when the pool is locked or before first unlock. SAS drives are out of scope.

## The stack

| Layer | Role |
| --- | --- |
| `drivetemp` (kernel) | Exposes each SATA drive's SMART temp as an hwmon input |
| Super I/O driver (kernel) | Board-specific (`nct6775`, `f71882fg`, `it87`, ...) -- drives the chassis fan PWM headers |
| `lm_sensors` (userspace) | Provides `sensors`, `sensors-detect`, `pwmconfig` for discovery |
| `hddfancontrol` (userspace) | Reads drivetemp hwmon inputs for all SATA drives, ramps PWM from the hottest |
| `braid.fanControl` (NixOS) | Runs `hddfancontrol` as a systemd service, handles SATA hotswap and crash recovery |

Setup has two phases: interactive discovery on the running machine (one-time), then committing the result to Nix.

## Prerequisites

- BIOS: put chassis fan headers into software/manual control, and match the header mode to the fan type -- **PWM for 4-pin fans, DC (voltage) for 3-pin fans**. Getting this wrong leaves the fan either stuck at a fixed speed or uncontrollable from userspace. If unsure, `pwmconfig`'s spin-down test (below) will tell you: a fan on the wrong header mode will not ramp down.
- Leave the CPU fan header on BIOS auto. Don't fight the board's package thermal logic with userspace -- the BIOS is better at protecting the CPU than you are.

## Discovery

Discovery is a one-time interactive procedure. Its only output is four values you paste into Nix at the end:

- `pwm.platformDevice` -- platform device name of the Super I/O chip (e.g. `f71882fg.656`)
- `pwm.number` -- PWM channel number on that chip (e.g. `2` for `pwm2`)
- `pwm.minStart` -- PWM value needed to start the fan from standstill
- `pwm.maxStop` -- PWM value below which the spinning fan stalls

### Install the tooling and load the sensor modules

`braid.fanControl` loads `drivetemp` automatically, but the interactive operator tools (`sensors`, `sensors-detect`, `pwmconfig`, `hddfancontrol`) are only needed on your PATH during discovery. Add them temporarily, plus your board's Super I/O driver -- these can stay in the committed config so future re-runs after drive swaps or chassis changes have the same tools available:

```nix
{ pkgs, ... }:
{
  environment.systemPackages = [ pkgs.lm_sensors pkgs.hddfancontrol ];
  boot.kernelModules = [ "coretemp" ];  # drivetemp added by braid.fanControl
}
```

Rebuild, then confirm you see per-drive temps:

```sh
sensors | grep -A1 drivetemp
```

You should see one `drivetemp-scsi-*-0` block per SATA drive, each showing a current `temp1` reading. `drivetemp` must be loaded **before** you run `pwmconfig`, or drive temps will not appear as eligible fan inputs.

### Find your Super I/O chip

Run `sudo sensors-detect` and accept the defaults. When it asks whether to write `/etc/modules-load.d/lm_sensors.conf`, **answer no** -- on NixOS, kernel modules are declared in `boot.kernelModules`, not in `/etc`.

At the end `sensors-detect` prints a summary. For most boards it names a driver (`nct6775`, `it87`, ...); add that driver to `boot.kernelModules` alongside `coretemp`, rebuild, and confirm a new block appears in `sensors` showing fan RPMs and PWMs.

If the summary says `Found unknown chip with ID 0xXXXX`, `sensors-detect`'s chip-ID table has fallen behind the kernel. The kernel driver may already support your chip even though the detect script doesn't recognize it. Grep the ID in the kernel source to find the driver:

```sh
# on github, search drivers/hwmon/*.c in torvalds/linux for the ID
# e.g., 0x1502 turns up in drivers/hwmon/f71882fg.c, so the module is f71882fg
```

Add the module you found to `boot.kernelModules`. If `modinfo <module>` works and `sensors` still shows no new block after rebuild, move on to the next section.

### "Device or resource busy" on module load

If `dmesg` shows your Super I/O driver correctly identifying the chip but `modprobe` fails with `Device or resource busy`, ACPI has reserved the hwmon I/O region. The fix is a kernel parameter:

```nix
boot.kernelParams = [ "acpi_enforce_resources=lax" ];
```

This requires a full reboot -- kernel command line changes don't apply on `nixos-rebuild switch` alone. After the reboot, `sensors` should show a block for your Super I/O chip with fan RPMs and PWMs.

### Map fans to PWMs with pwmconfig

`pwmconfig` identifies which PWM controls which fan by briefly stopping each fan in turn. Run it when drives are idle (not mid-scrub or rebuild) -- a stalled fan during sustained write load is a bad place to be.

Before starting, record each PWM's current `enable` value. `pwmconfig` flips them to manual (1) to run its spin-down test, and the meaning of other values is driver-specific (e.g. `f71882fg` uses 0=off / 1=manual / 2=auto; other drivers differ). Restoring the original is safer than hard-coding a mode:

```sh
for p in /sys/class/hwmon/*/device/pwm[0-9]_enable; do
  printf '%s = %s\n' "$p" "$(cat "$p")"
done
```

Save that output somewhere you can read after `pwmconfig` exits. Then run:

```sh
sudo pwmconfig
```

It walks each PWM, asks whether to switch it to manual (say yes so the spin-down test can run), then stops each fan briefly and asks which `fanN_input` reading dropped. Answer based on what you observe in the tool's output.

After identification, it asks which fans to configure. Pick only the chassis fans. **Skip the CPU PWM** -- leave it BIOS-controlled. Also skip any PWM whose fan did not respond (unpopulated header, or fan/header mode mismatch in BIOS).

`pwmconfig` writes an `/etc/fancontrol` file at the end. You won't use that file (braid uses `hddfancontrol`, not vanilla `fancontrol`), but the tool's spin-down output is still how you identify the PWM path and measure stall behavior. Record the PWM sysfs path for the chassis fan -- something like `/sys/devices/platform/<super-io>/hwmon/hwmonN/device/pwmN`.

### Translate the PWM path to a platform device

`braid.fanControl` takes the stable platform device name plus the PWM channel number, and resolves the (unstable) `hwmonN` segment at service start. Translate the pwmconfig-surfaced sysfs path with:

```sh
pwm=/sys/class/hwmon/hwmon4/device/pwm2  # from pwmconfig output
pwm_dir=$(dirname "$pwm")
if [ "$(basename "$pwm_dir")" != device ]; then
  pwm_dir="$pwm_dir/device"
fi
basename "$(readlink -f "$pwm_dir")"
# -> f71882fg.656
```

The `if` branch handles both sysfs layouts: `hwmon*/device/pwmN` (common on f71882fg, nct6775) and `hwmon*/pwmN` (fallback). Without it, the fallback layout resolves to `hwmon4` instead of the platform device.

The PWM number is the numeric suffix on the `pwmN` filename (`2` in the example above).

After `pwmconfig` exits, restore each skipped PWM to the value you recorded:

```sh
echo <original> | sudo tee /sys/class/hwmon/<N>/device/pwmK_enable
```

### Measure minStart and maxStop with hddfancontrol pwm-test

`hddfancontrol pwm-test` ramps the PWM up and down while measuring fan RPM. It finds:

- `pwm.minStart` -- the PWM at which a stopped fan begins spinning again
- `pwm.maxStop` -- the highest PWM at which a spinning fan stalls

Run it against the chassis PWM path from the previous step:

```sh
sudo hddfancontrol pwm-test -p /sys/devices/platform/.../pwmN
```

It takes a couple of minutes (ramps slowly to avoid bouncing the fan). Record the final `minStart` and `maxStop` values it prints.

**If the fan has a hardware RPM floor** (common on voltage-controlled 3-pin fans, and some boards' chassis headers even in PWM mode), `pwm.maxStop` will be 0 and `pwm.minStart` will be some low value -- the fan never actually stops. That's fine; `hddfancontrol` still handles the ramp correctly. The `--min-fan-speed-prct` floor in `braid.fanControl` prevents the daemon from commanding the fan off in any case.

## Committing to Nix

Fan control is a braid sub-feature: it activates only when `braid.enable = true` (see [Getting started](getting-started.md)). The recipes below show the full `braid` block; merge the non-`braid` lines (`boot.*`, `environment.systemPackages`) into your existing config.

### Minimal recipe

Paste the four discovery values into `braid.fanControl`:

```nix
{ pkgs, ... }:
{
  environment.systemPackages = [ pkgs.lm_sensors ];   # optional: tools for re-running discovery
  boot.kernelModules = [ "coretemp" "nct6775" ];      # your Super I/O driver here
  # boot.kernelParams = [ "acpi_enforce_resources=lax" ];  # only if needed

  braid = {
    enable = true;            # fan control only runs when the braid module is enabled

    fanControl = {
      enable = true;
      pwm = {
        platformDevice = "nct6775.656";
        number = 2;
        minStart = 65;   # from hddfancontrol pwm-test
        maxStop  = 60;   # from hddfancontrol pwm-test
      };
    };
  };
}
```

The module resolves the PWM sysfs path at service start by globbing `/sys/devices/platform/<platformDevice>/hwmon/hwmon*/{device/,}pwm<number>`, which handles `hwmonN` renumbering across reboots. The platform device name (`nct6775.656`, `f71882fg.656`, etc.) is stable.

Sane defaults for the rest:

- `minTemp = 30` / `maxTemp = 40` -- fan floors below 30 C, ramps to full at 40 C
- `minFanSpeedPercent = 20` -- fan never drops below 20% of range (conservative; upstream hddfancontrol default)
- `interval = "30s"` -- 30-second polling interval

Override any of these in the `braid.fanControl` block if you want a different curve. See [NixOS configuration](nixos-configuration.md) for the full option table.

### Tuning the curve

- **Ramp starts too soon / fan audibly spools early on idle:** raise `minTemp` (try 32-34).
- **Drives climbing past 42-44 C under sustained load:** lower `maxTemp` (try 38) or raise `minFanSpeedPercent` (try 30).
- **Fan noticeably oscillating:** raise `interval` (e.g. `"60s"`). HDDs heat slowly, so aggressive polling only adds jitter.

### Additional sensor modules

For ECC DIMM temp monitoring (visible in `sensors`; not used by `hddfancontrol` directly):

```nix
boot.kernelModules = [ "coretemp" "nct6775" "jc42" ];
```

## Verification

Watch **both** the drivetemp input and the PWM/RPM, not RPM alone. CPU heat or ambient temperature can produce a false-positive fan ramp if you're eyeballing only RPM.

The self-contained recipe for a braid NAS (btrfs is already assumed): run a scrub as the heat source. It reads every extent on every drive, which is representative NAS load and needs no pre-staged payload. The example below uses `/mnt/storage` as a concrete mount point -- substitute your own pool mount:

```sh
# pane 1: start the scrub
sudo btrfs scrub start /mnt/storage

# pane 2: watch the thermal signals
watch -n2 sensors

# pane 3: follow the hddfancontrol daemon log
journalctl -u hddfancontrol-braid -f
```

Expected: drive temps climb 3-8 C over 10+ minutes (HDDs heat slowly), and the PWM tracks in step per your `minTemp`/`maxTemp` curve. The daemon log prints temperature readings and speed changes as it polls.

Cancel anytime with:

```sh
sudo btrfs scrub cancel /mnt/storage
```

If drive temps climb but PWM doesn't move, double-check the resolved PWM file is writable (`ls -l /sys/devices/platform/<platformDevice>/hwmon/hwmon*/{device/,}pwm<number>`) and that `hddfancontrol-braid` is running (`systemctl status hddfancontrol-braid`).

## Monitoring commands

Quick reference for monitoring the fan control loop on a running system. These paths assume an `f71882fg`-family Super I/O; substitute your platform device if different.

```sh
# Live chassis fan RPM + drive temps
watch -n2 'cat /sys/devices/platform/f71882fg.656/fan2_input; sensors drivetemp-*'

# Follow daemon log (temp readings, speed changes)
journalctl -u hddfancontrol-braid -f

# Current PWM value (0-255)
cat /sys/devices/platform/f71882fg.656/pwm2

# All fan channels at a glance (RPM, PWM, control mode)
for i in 1 2 3; do echo "fan${i}: $(cat /sys/devices/platform/f71882fg.656/fan${i}_input) RPM, pwm${i}: $(cat /sys/devices/platform/f71882fg.656/pwm${i}), enable: $(cat /sys/devices/platform/f71882fg.656/pwm${i}_enable)"; done

# Service status
systemctl status hddfancontrol-braid

# All hwmon sensors (CPU, board, DIMM, drives)
sensors

# SMART details for a specific drive
sudo smartctl -a /dev/sda
```

The `pwmN_enable` values: 0=off, 1=manual (hddfancontrol sets this), 2=BIOS auto. hddfancontrol is configured with `--restore-fan-settings`, so a clean service stop restores the original enable mode.

## TUI fans panel

`braid tui`'s Data tab gains a Fans row when fan control is enabled. The
section title shows `daemon:` status for `hddfancontrol-braid.service`; the
row shows current PWM/RPM, the Driving column names the hottest drive setting
the curve, and the Curve column shows the configured temperature-to-speed
range. The panel polls every 5 seconds. Press `r` to refresh both pool and fan
probes immediately.

## When braid.fanControl isn't enough

`braid.fanControl` drives a single chassis PWM from the hottest SATA drive. That covers the common NAS case. If you need more control -- multiple PWMs with different curves, PID-based responsiveness, non-SATA drive temperature sources -- the usual escape hatches:

- Configure `services.hddfancontrol` directly (nixpkgs's module supports multiple daemons, per-fan config).
- [`fan2go`](https://github.com/markusressel/fan2go) -- Go daemon; supports multiple sensors and PID curves.
- [CoolerControl](https://docs.coolercontrol.org/) -- more featureful, GUI-oriented.

Disable `braid.fanControl.enable` and bring your own solution. The `drivetemp` kernel module that braid loads is the only piece you'd need to keep.

## Worked example: ASRock Industrial IMB-X1231

A concrete walk-through of the discovery phase on one board, to show what the unknown-chip and ACPI-busy paths look like in practice.

### Hardware

- Board: ASRock Industrial IMB-X1231 (mini-ITX, 12th/13th gen Intel)
- CPU: Intel i3-14100T (35W TDP)
- Memory: ECC SODIMM with a jc42-compatible thermal sensor on SMBus
- Chassis fan: single 120mm rear, voltage-controlled (not 4-pin PWM)

### sensors-detect reported an unknown chip

```
Probing for Super-I/O at 0x2e/0x2f
Trying family `VIA/Winbond/Nuvoton/Fintek'...               Yes
Found unknown chip with ID 0x1502
    (logical device 4 has address 0x290, could be sensors)
...
Probing for `National Semiconductor LM78' at 0x290...       Success!
    (confidence 6, driver `lm78')
```

The `lm78` hit is a **false positive**: it's a 1995 chip whose ISA probe signature collides with anything living at 0x290. The real chip is whatever has devid 0x1502. Ignore the `lm78` recommendation.

### Finding the right driver in kernel source

A grep of `drivers/hwmon/` in torvalds/linux for `0x1502` pointed at `f71882fg.c`:

```c
#define SIO_F81866_ID    0x1010
#define SIO_F81966_ID    0x1502
/* ... */
case SIO_F81866_ID:
case SIO_F81966_ID:
```

So: Fintek F81966, register-compatible with the F81866, driven by the `f71882fg` module -- which has supported this ID since kernel 5.16. lm_sensors 3.6.2's chip-ID table just hadn't caught up.

### ACPI held the hwmon I/O region

After adding `f71882fg` to `boot.kernelModules` and rebuilding, the driver identified the chip in `dmesg`:

```
f71882fg: Found f81866a chip at 0x290, revision 48, devid: 1502
```

But `modprobe` failed with `Device or resource busy`. Added `boot.kernelParams = [ "acpi_enforce_resources=lax" ]`, rebooted, and `sensors` showed the full Super I/O block:

```
f81866a-isa-0290
  fan1: 1489 RPM       <- rear chassis, voltage-controlled
  fan2: 1573 RPM       <- CPU cooler
  fan3:    0 RPM       <- unpopulated header
  pwm1: 58%  pwm2: 58%  pwm3: 72%
  temp1: 36.0 C  temp2: 20.0 C  temp3: 37.0 C
```

### pwmconfig mapped the fans

The spin-down test confirmed:

- `pwm2 -> fan2` (chassis fan; voltage-controlled -- RPM floors at ~395 and never fully stops regardless of PWM). Selected for braid.fanControl.
- `pwm1 -> fan1` (4-pin PWM CPU fan; stopped cleanly at PWM=60). Skipped -- left on BIOS auto.
- `pwm3 -> no fan` (unpopulated header). Also left on auto.

### hddfancontrol pwm-test

```
sudo hddfancontrol pwm-test -p /sys/devices/platform/f71882fg.656/hwmon/hwmon4/device/pwm2
...
minStart: 65
maxStop:  60
```

### Final Nix config

```nix
boot.kernelModules = [ "coretemp" "f71882fg" "jc42" ];
boot.kernelParams  = [ "acpi_enforce_resources=lax" ];

braid = {
  enable = true;

  fanControl = {
    enable = true;
    pwm = {
      platformDevice = "f71882fg.656";
      number = 2;
      minStart = 65;
      maxStop  = 60;
    };
  };
};
```

### End-to-end check

With drive temp at 32 C and the default curve (`minTemp=30`, `maxTemp=40`, `minFanSpeedPercent=20`): expected `pwm2` = `20% + (32 - 30) / (40 - 30) * 80%` of PWM range above `maxStop`. Observed `pwm2` climbed smoothly with drive temp under a scrub, RPM tracked the `pwmconfig` correlation table. Control loop arithmetically correct.

## What's next

- [Power management](power-management.md) -- suspend/resume and WoL, which interact with fan control
- [Monitoring and alerts](monitoring-and-alerts.md) -- SMART-based alerting complements active cooling

## Related

- [Arch Wiki: Fan speed control](https://wiki.archlinux.org/title/Fan_speed_control) -- distro-neutral reference for lm_sensors and fan control
- [Kernel `drivetemp` driver](https://docs.kernel.org/hwmon/drivetemp.html) -- what the module exposes
- [`hddfancontrol`](https://github.com/desbma/hddfancontrol) -- upstream project
