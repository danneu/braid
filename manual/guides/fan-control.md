[← Manual](../index.md)

# Fan control

This guide covers how to drive chassis fans from HDD temperatures on a NixOS NAS, using the kernel `drivetemp` module plus `lm_sensors` and `fancontrol`.

Read this if you want quieter idle and predictable ramp under sustained disk load -- BIOS fan curves cannot see HDD temperatures, only CPU and motherboard temperatures.

## Why HDD-driven fan control

On a NAS the dominant thermal load is the HDDs, not the CPU. A pool of spinning drives under scrub or heavy write load runs much hotter for much longer than a low-TDP NAS CPU ever does. BIOS fan curves only see CPU package temp and a motherboard sensor -- they cannot see drive temp. So chassis fans controlled by the BIOS ramp for the wrong reason at the wrong time.

The fix is to move fan control into Linux userspace with `fancontrol`, using drive temps as the signal. The kernel's `drivetemp` module exposes each SATA drive's SMART temperature as a standard hwmon input, which `fancontrol` can read and use to drive a chassis fan's PWM.

braid itself does not manage fans. This is pure NixOS hardware setup that happens alongside braid on a NAS.

## The stack

Five layers cooperate:

| Layer | Role |
| --- | --- |
| `drivetemp` (kernel) | Exposes each SATA drive's SMART temp as an hwmon input |
| Super I/O driver (kernel) | Board-specific (`nct6775`, `f71882fg`, `it87`, ...) -- drives the chassis fan PWM headers |
| `lm_sensors` (userspace) | Provides `sensors`, `sensors-detect`, `pwmconfig` for discovery |
| `fancontrol` (userspace) | Shell daemon that reads temps and writes PWM values per a curve |
| `hardware.fancontrol` (NixOS) | Runs `fancontrol` as a systemd service, handles suspend/resume |

Setup has two phases: interactive discovery on the running machine (one-time), then committing the result to Nix.

## Prerequisites

- BIOS: put chassis fan headers into software/manual control, and match the header mode to the fan type -- **PWM for 4-pin fans, DC (voltage) for 3-pin fans**. Getting this wrong leaves the fan either stuck at a fixed speed or uncontrollable from userspace. If unsure, `pwmconfig`'s spin-down test (below) will tell you: a fan on the wrong header mode will not ramp down.
- Leave the CPU fan header on BIOS auto. Don't fight the board's package thermal logic with userspace -- the BIOS is better at protecting the CPU than you are.

## Discovery

Discovery is a one-time interactive procedure. Its only output is values you paste into Nix at the end.

### Install the tooling and load the sensor modules

The NixOS `hardware.fancontrol` module ships the `fancontrol` daemon, but does **not** put the interactive operator tools (`sensors`, `sensors-detect`, `pwmconfig`) on PATH. Add them explicitly, and keep them in your committed config -- future re-runs after drive swaps or chassis changes need the same tools:

```nix
{ pkgs, ... }:
{
  environment.systemPackages = [ pkgs.lm_sensors ];
  boot.kernelModules = [ "coretemp" "drivetemp" ];
}
```

Rebuild, then confirm you see per-drive temps:

```sh
sensors | grep -A1 drivetemp
```

You should see one `drivetemp-scsi-*-0` block per SATA drive, each showing a current `temp1` reading. `drivetemp` must be loaded **before** you run `pwmconfig`, or drive temps will not appear as eligible fan inputs.

### Find your Super I/O chip

Run `sudo sensors-detect` and accept the defaults. When it asks whether to write `/etc/modules-load.d/lm_sensors.conf`, **answer no** -- on NixOS, kernel modules are declared in `boot.kernelModules`, not in `/etc`.

At the end `sensors-detect` prints a summary. For most boards it names a driver (`nct6775`, `it87`, ...); add that driver to `boot.kernelModules` alongside `coretemp` and `drivetemp`, rebuild, and confirm a new block appears in `sensors` showing fan RPMs and PWMs.

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

When it asks for a temperature input, pick any drivetemp node -- you'll finalize this in the Nix config. Enter your `MINTEMP` (fan at floor, e.g. 30) and `MAXTEMP` (fan at full, e.g. 40), accept the default `MAXPWM=255`. `pwmconfig` writes the result to `/etc/fancontrol`.

After `pwmconfig` exits, restore each skipped PWM to the value you recorded:

```sh
echo <original> | sudo tee /sys/class/hwmon/<N>/device/pwmK_enable
```

### Identifying the pilot drive

Vanilla fancontrol supports **one temperature input per PWM** -- that's a config-format limitation. For a multi-drive pool, you have to pick one drive to be the pilot.

Heuristic: pick the drive that runs hottest in sustained ops, typically a middle bay (worst airflow, highest steady-state temp). `FCTEMPS=` takes an hwmon path, not a block device, so record the mapping from hwmon path to serial to physical bay once, so it survives drive swaps and re-cabling:

```sh
for h in /sys/class/hwmon/hwmon*; do
  [ "$(cat "$h/name" 2>/dev/null)" = drivetemp ] || continue
  blk=$(ls "$h/device/block" 2>/dev/null | head -1)
  [ -n "$blk" ] || continue
  serial=$(sudo smartctl -i "/dev/$blk" | awk -F: '/Serial Number/ {gsub(/ /,""); print $2}')
  echo "$h -> /dev/$blk (serial $serial)"
done
```

Write the result down somewhere persistent -- a comment in the Nix config, a README in the pool, a sticker on the chassis. Drive serials don't change; `/dev/sdX` names do.

## Committing to Nix

### Minimal recipe

Once `/etc/fancontrol` exists and looks right, paste its contents into `hardware.fancontrol.config`. Paste **verbatim**, including the `DEVPATH=` and `DEVNAME=` lines -- they pin to stable bus paths and survive `hwmonN` renumbering across reboots. Don't be tempted to "clean them up".

```nix
{ pkgs, ... }:
{
  environment.systemPackages = [ pkgs.lm_sensors ];
  boot.kernelModules = [ "coretemp" "drivetemp" "nct6775" ];  # your SIO driver here
  # boot.kernelParams = [ "acpi_enforce_resources=lax" ];     # only if needed

  hardware.fancontrol = {
    enable = true;
    config = ''
      INTERVAL=10
      DEVPATH=hwmon2=devices/pci0000:00/.../ata5/... hwmon5=devices/platform/nct6775.656
      DEVNAME=hwmon2=drivetemp hwmon5=nct6775
      FCTEMPS=hwmon5/device/pwm1=hwmon2/temp1_input
      FCFANS= hwmon5/device/pwm1=hwmon5/device/fan1_input
      MINTEMP=hwmon5/device/pwm1=30
      MAXTEMP=hwmon5/device/pwm1=40
      MINSTART=hwmon5/device/pwm1=150
      MINSTOP=hwmon5/device/pwm1=0
    '';
  };
}
```

The `DEVPATH=`, `DEVNAME=`, and `FCTEMPS=` values above are illustrative -- use the ones `pwmconfig` wrote for your hardware.

### Tuning MINSTART and MINSTOP

Some chassis fans -- voltage-controlled 3-pin fans, and certain boards' chassis headers even in PWM mode -- have a hardware RPM floor. Writing `PWM=0` doesn't stop the fan; it holds at the floor (often 300-500 RPM).

Read the correlation table that `pwmconfig` printed during its spin-down test. Find the PWM below which RPM stops decreasing:

```
PWM 30 FAN 452
PWM 28 FAN 409
PWM 26 FAN 399      <-- below here, RPM is the floor
PWM 24 FAN 396
PWM 22 FAN 397
```

Set `MINSTOP` to the floor PWM (26 in this example). `MINSTART` is the PWM needed to start a fully-stopped fan; on a fan with a hardware floor it never fully stops, so `MINSTART` is effectively dormant -- accept whatever `pwmconfig` chose.

### Additional sensor modules

For ECC DIMM temp monitoring (useful on ECC builds; not used by `fancontrol` directly but visible in `sensors`):

```nix
boot.kernelModules = [ "coretemp" "drivetemp" "nct6775" "jc42" ];
```

## Verification

Watch **both** the drivetemp input and the PWM/RPM, not RPM alone. CPU heat or ambient temperature can produce a false-positive fan ramp if you're eyeballing only RPM.

The self-contained recipe for a braid NAS (btrfs is already assumed): run a scrub as the heat source. It reads every extent on every drive, which is representative NAS load and needs no pre-staged payload. The example below uses `/mnt/storage` as a concrete mount point -- substitute your own pool mount:

```sh
# pane 1: start the scrub
sudo btrfs scrub start /mnt/storage

# pane 2: watch the thermal signals
watch -n2 sensors
```

Expected: the chosen drivetemp climbs 3-8 C over 10+ minutes (HDDs heat slowly), and the PWM tracks in step per your `MINTEMP`/`MAXTEMP` curve. Cancel anytime with:

```sh
sudo btrfs scrub cancel /mnt/storage
```

If drive temp climbs but PWM doesn't move, `FCTEMPS=` points at the wrong hwmon. Cross-check with `cat /sys/class/hwmon/*/name` on the running system.

## After a drive swap or re-cable

`DEVPATH=hwmonN=devices/pci.../ataM/...` pins the temperature source to a **SATA port**, not a drive. It survives reboots and hwmonN renumbering, but is wrong after any physical reshuffle -- the fan will track whatever drive now sits in that port, which may no longer be your hottest or most-loaded drive.

After a drive swap or re-cable:

1. Re-run the hwmon-to-serial mapping from the "Identifying the pilot drive" section above.
2. Re-verify that `FCTEMPS=` points at the hwmon for the drive you want to pilot from. If it doesn't, re-run `pwmconfig` or edit the path in place.
3. Rebuild, then re-run the verification loop to confirm the signal path is live end-to-end.

## Suspend, resume, and failure behavior

A few things `hardware.fancontrol` handles for you, plus one it doesn't:

- **Suspend/resume**: the NixOS module's `powerManagement.resumeCommands` restarts `fancontrol.service` after resume. Suspend works out of the box.
- **Crash**: the service has `Restart=on-failure`, so a crashed `fancontrol` restarts itself.
- **Manual stop**: if you stop the service (not a crash), PWMs stay at whatever value was last written. The fan will **not** ramp under new thermal load. Always `systemctl restart fancontrol` after any manual intervention with the service or with `/sys/class/hwmon/...` paths.

## When vanilla fancontrol isn't enough

Vanilla fancontrol's one-input-per-PWM limit is fine for most small NAS builds. If you outgrow it -- many drives, PID-based curves, per-drive responsiveness -- the common escape hatches are:

- [`hddfancontrol`](https://github.com/desbma/hddfancontrol) -- purpose-built for HDD-driven fan control; reads SMART temp from N drives, aware of spin-down states.
- [`fan2go`](https://github.com/markusressel/fan2go) -- Go daemon; supports multiple sensors and PID curves.
- [CoolerControl](https://docs.coolercontrol.org/) -- more featureful, GUI-oriented.

Adopt one of these and replace `hardware.fancontrol.enable` if you need that level of control. Not covered further here.

## What's next

- [Power management](power-management.md) -- suspend/resume and WoL, which interact with fan control
- [Monitoring and alerts](monitoring-and-alerts.md) -- SMART-based alerting complements active cooling

## Related

- [Arch Wiki: Fan speed control](https://wiki.archlinux.org/title/Fan_speed_control) -- distro-neutral reference for lm_sensors and fancontrol
- [Kernel `drivetemp` driver](https://docs.kernel.org/hwmon/drivetemp.html) -- what the module exposes
- [fancontrol(8)](https://man.archlinux.org/man/extra/lm_sensors/fancontrol.8.en) -- config file format reference
