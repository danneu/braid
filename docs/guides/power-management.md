[← braid](../index.md)

# Power management

This guide covers auto-suspend, Wake-on-LAN (WoL), and troubleshooting the hardware and software chain that makes it all work.

Read this if you want your NAS to sleep when idle and wake on demand.

## How auto-suspend works

When enabled, braid uses [autosuspend](https://github.com/languitar/autosuspend) to suspend the entire NAS to RAM when idle. This stops all drives, the CPU, and fans -- the machine draws almost no power. Wake it with a Wake-on-LAN magic packet from any device on the network.

Suspend-to-RAM preserves LUKS keys and the mounted btrfs pool in memory. When the NAS wakes, the pool is immediately available -- no re-unlock needed.

### What counts as activity

The NAS stays awake while any of these are true:

| Check | What it detects |
| --- | --- |
| **braid idle** | scrub plus any btrfs kernel exclusive operation (balance, device add/remove/replace, resize, swap activate) -- the latter via `/sys/fs/btrfs/<fsid>/exclusive_operation` |
| **braid wol-ready** | configured wired NIC currently reports `Wake-on: g`; if WoL is disabled or unverifiable, auto-suspend is blocked |
| **SSH** | Active SSH connections (port 22) |
| **Local sessions** | TTY, X11, or Wayland sessions (via logind) |
| **Samba** | Active SMB clients (auto-detected, only if Samba is enabled) |
| **NFS** | Active NFS connections on port 2049 (auto-detected, only if NFS server is enabled) |

If all checks pass (everything idle) for the configured idle time (default 15 minutes), the NAS suspends.

The WoL check gates braid's auto-suspend path only. Manual `sudo systemctl suspend` remains available for maintenance and testing, but it bypasses braid's pre-suspend WoL check.

### Scrub wakeups

The monthly btrfs scrub timer is registered as an autosuspend wakeup source. If the NAS is asleep when a scrub is due, it wakes via RTC alarm, runs the scrub, and suspends again when idle.

## Configuration

```nix
braid = {
  enable = true;

  autoSuspend = {
    enable = true;
    idleTime = 900;          # seconds before suspend (default: 900 = 15 min)
    wolInterface = "eno1";   # required -- your wired ethernet interface
  };
};
```

### Options

| Option | Default | Description |
| --- | --- | --- |
| `autoSuspend.enable` | `false` | Enable auto-suspend when idle |
| `autoSuspend.idleTime` | `900` | Seconds of idle time before suspending |
| `autoSuspend.wolInterface` | -- | Wired ethernet interface for Wake-on-LAN (required) |

`wolInterface` is mandatory. Without WoL, a suspended NAS is unreachable until someone presses the power button. Find your interface with:

```sh
ip link
```

Look for your wired ethernet interface (usually `eno1`, `enp1s0`, or similar). WiFi interfaces (`wl*`) are rejected -- WoL requires wired ethernet.

## Hardware compatibility

### Recommended: Intel NICs

Intel ethernet controllers (e.g., X540, I210, I225) have reliable WoL support with the in-kernel `ixgbe` and `igc` drivers. These are the lowest-risk choice for a NAS that needs reliable remote wakeup.

### Avoid: Aquantia/Marvell AQC107

The AQC107 (atlantic driver) has known WoL reliability issues on Linux. If WoL is important to you, avoid this chipset.

### RTL8125 (Realtek 2.5GbE)

The Realtek RTL8125 works for WoL but requires the vendor `r8125` driver instead of the in-kernel `r8169`. See the troubleshooting section below.

## WoL troubleshooting

WoL involves a chain from BIOS to NIC driver to PCI bridge. When it does not work, you need to figure out which link in the chain is broken. Work through these steps in order.

### 1. Check BIOS settings

WoL must be enabled in your BIOS/UEFI. The exact option names vary by vendor, but look for:

- **Wake on LAN** or **Wake on PCI/PCIe** -- enable this.
- **ErP Ready** or **ErP Lot 6** -- disable this. ErP is an EU power-saving regulation that cuts standby power below what the NIC needs to listen for magic packets. If ErP is on, WoL cannot work.
- **Deep Sleep** -- disable if present. Similar to ErP, this cuts power to PCIe slots during standby.

### 2. Test basic suspend first

Before debugging WoL, verify the NAS can suspend and wake at all:

```sh
# Check available sleep states
cat /sys/power/state
```

You should see `mem` in the output. Test a manual suspend and wake with the power button:

```sh
sudo systemctl suspend
```

Press the power button to wake. If this does not work, suspend itself is broken (check ACPI settings in BIOS).

#### Identify spurious wake sources

Sometimes the NAS wakes immediately after suspend. Check what woke it:

```sh
# What woke the system last time?
journalctl -b -k | grep -i "wake"

# List ACPI wake sources
cat /proc/acpi/wakeup
```

The output looks like:

```
Device  S-state   Status   Sysfs node
XHC0      S3    *enabled   pci:0000:00:14.0
GLAN      S4    *enabled   pci:0000:00:1f.6
...
```

Disable wake sources one at a time to find the culprit. Use a binary search -- disable half, test, narrow down.

To disable a wake source temporarily (resets on reboot):

```sh
echo XHC0 | sudo tee /proc/acpi/wakeup
```

Once you find the problematic device, disable it permanently via a udev rule in your NixOS config:

```nix
services.udev.extraRules = ''
  # Disable USB controller wake (XHC0 causes spurious wakeups)
  ACTION=="add", SUBSYSTEM=="pci", ATTR{vendor}=="0x8086", ATTR{device}=="0xa0ed", ATTR{power/wakeup}="disabled"
'';
```

Find the vendor and device IDs from `lspci -nn` for the corresponding PCI device.

### 3. Verify WoL is enabled on the NIC

After rebuild with `braid.autoSuspend.wolInterface` set, verify with doctor:

```sh
sudo braid doctor
```

Expected row:

```
[ok]   wake-on-lan     eno1 reports Wake-on: g (magic packet armed)
```

`Wake-on: g` means WoL is active (magic packet mode). If doctor reports `Wake-on: d` (disabled), the NixOS config is not taking effect -- check that you rebuilt, that the interface name is correct, and that BIOS/driver WoL settings allow wake.

With `autoSuspend` enabled, braid also checks this before every automatic suspend. If the NAS is idle but does not sleep, run `sudo braid doctor` and inspect the `wake-on-lan` row.

### 4. Test WoL from another machine

From a different machine on the same network, send a magic packet:

```sh
# Install wakeonlan tool (on the sending machine)
# NixOS: nix-shell -p wakeonlan
# macOS: brew install wakeonlan

# Get the NAS MAC address (on the NAS, before suspending)
ip link show eno1
# look for "link/ether xx:xx:xx:xx:xx:xx"

# Suspend the NAS
ssh user@nas sudo systemctl suspend

# Send the magic packet (from the other machine)
wakeonlan xx:xx:xx:xx:xx:xx
```

If the NAS wakes, WoL is working. If not, continue to the next steps.

### 5. NIC driver issues

#### RTL8125 (Realtek 2.5GbE)

The in-kernel `r8169` driver handles the RTL8125 but has unreliable WoL. The vendor `r8125` driver fixes this.

Add to your NixOS config:

```nix
boot.extraModulePackages = with config.boot.kernelPackages; [ r8125 ];
boot.blacklistedKernelModules = [ "r8169" ];
```

After rebuild, verify the driver:

```sh
ethtool -i eno1 | grep driver
# should show: driver: r8125
```

### 6. PCI bridge wakeup (PME propagation)

Even with WoL enabled on the NIC, the NIC's wake signal (PME -- Power Management Event) must propagate through the PCI bridge to reach the CPU. Some BIOS implementations do not enable PME on intermediate bridges.

Check if PME is enabled on the bridge:

```sh
# Find the NIC's PCI address
lspci | grep -i ethernet
# e.g., 05:00.0 Ethernet controller: Intel ...

# Find its parent bridge
lspci -t
# Look for the tree path to your NIC

# Check PME on the bridge
sudo lspci -vvs 00:1c.0 | grep -i pme
# Look for "PME-Enable+" (good) or "PME-Enable-" (bad)
```

If PME is disabled on the bridge, enable it with a udev rule:

```nix
services.udev.extraRules = ''
  # Enable PME on PCI bridge for NIC WoL
  ACTION=="add", SUBSYSTEM=="pci", ATTR{vendor}=="0x8086", ATTR{device}=="0x7ab8", RUN+="${pkgs.pciutils}/bin/setpci -s %k CAP_PM+04.W=0100:0100"
'';
```

The `setpci` command sets the PME_En bit in the PCI PM capability. Replace the vendor/device IDs with those from your bridge:

```sh
sudo lspci -nn -s 00:1c.0
# e.g., 00:1c.0 PCI bridge [0604]: Intel Corporation ... [8086:7ab8]
```

#### Finding the right bridge

If `lspci -t` is hard to read, use this to trace the full path from NIC to root:

```sh
# Starting from the NIC PCI address (e.g., 05:00.0)
cd /sys/bus/pci/devices/0000:05:00.0
ls -la ..  # parent bridge
# Follow symlinks up until you reach the root bridge
```

Each bridge in the chain must have PME enabled for WoL to work. In practice, it is usually only one bridge between the NIC and the root that needs fixing.

### 7. Still not working

If WoL still fails after all the above:

1. **Check `dmesg` on wake** -- after waking with the power button, look for clues:
   ```sh
   dmesg | grep -i -E "wake|suspend|pme|wol"
   ```

2. **Try a different NIC** -- if your motherboard has multiple ethernet ports, try WoL on each one. Onboard Intel NICs are most reliable.

3. **Test with a minimal NixOS config** -- remove all non-essential services and test WoL in isolation. If it works minimal but not full, bisect your config.

4. **Check the NIC firmware** -- some NICs need firmware loaded at boot for WoL. Check `dmesg | grep firmware` for errors.

## What's next

- [Monitoring and alerts](monitoring-and-alerts.md) -- disk health alerts
- [Day-to-day NAS usage](day-to-day-nas-usage.md) -- the reboot/unlock cycle

## Related commands

- [idle](../commands/idle.md)
- [status](../commands/status.md)
- [lock](../commands/lock.md)
