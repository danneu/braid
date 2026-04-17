[← Manual](../index.md)

# NixOS configuration

Complete reference for the braid NixOS module options. Read this when setting up braid for the first time or tuning behavior after initial setup.

## Minimal config

```nix
# flake.nix
{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";
    braid.url = "github:danneu/braid";
    braid.inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs = { nixpkgs, braid, ... }: {
    nixosConfigurations.myhost = nixpkgs.lib.nixosSystem {
      system = "x86_64-linux";
      modules = [
        braid.nixosModules.default
        ./configuration.nix
      ];
    };
  };
}
```

```nix
# configuration.nix
braid = {
  enable = true;
  package = braid.packages.x86_64-linux.default;
};
```

`braid.package` is required when `braid.enable = true`. The module will fail evaluation without it.

## What you get for free

When `braid.enable = true`, the module sets up:

- **Monthly btrfs scrub** -- timer + service tied to pool lifecycle. Configurable via `braid.autoScrub`.
- **Resilient boot** -- a dead drive never blocks boot. LUKS open and btrfs mount are deferred to `braid unlock`, not wired into `boot.initrd`.
- **Pinned toolchain** -- btrfs-progs, cryptsetup, and util-linux are pinned to NixOS stable versions. Override with `braid.packages.*` if needed.
- **Shell completions** -- bash, zsh, and fish completions registered automatically via `clap_complete`.
- **smartd integration** -- `services.smartd` enabled by default with a braid-owned alert script. SMART failures trigger the braid alert service.
- **Storage group** -- a `storage` group is created; mount point is set to `root:storage 2770` after unlock. See [Sharing and permissions](sharing-and-permissions.md).
- **Disk health monitoring** -- polls btrfs device stats every 5 minutes, audible beep on errors. Configurable via `braid.monitor`.
- **Fan control** (opt-in) -- drive chassis fans from the hottest SATA drive temp. Handles hddfancontrol, SATA hotswap restart, crash recovery. Configurable via `braid.fanControl`.

## Module options

### Core

| Option | Type | Default | Description |
| --- | --- | --- | --- |
| `braid.enable` | bool | `false` | Enable the braid module |
| `braid.package` | package or null | `null` | The braid CLI package (required when enabled) |
| `braid.mountPoint` | path | `/mnt/storage` | Where to mount the btrfs pool |
| `braid.storageGroup` | string or null | `"storage"` | Group for mount point access. `null` to disable |

### Tool overrides

| Option | Type | Default | Description |
| --- | --- | --- | --- |
| `braid.packages.cryptsetup` | package | `pkgs.cryptsetup` | cryptsetup package |
| `braid.packages.btrfsProgs` | package | `pkgs.btrfs-progs` | btrfs-progs package |
| `braid.packages.utilLinux` | package | `pkgs.util-linux` | util-linux package |

Override these only if you need a specific version for compatibility testing. The defaults are the NixOS stable versions from your nixpkgs input.

### Auto-scrub

| Option | Type | Default | Description |
| --- | --- | --- | --- |
| `braid.autoScrub.enable` | bool | `true` | Enable periodic btrfs scrub |
| `braid.autoScrub.interval` | string | `"monthly"` | systemd calendar expression |

The scrub timer is lifecycle-aware: it starts when the pool comes online and stops when the pool goes offline. `Persistent = true` ensures a missed scrub runs on next unlock (e.g. the pool was locked over a monthly boundary).

braid's scrub conflicts with the NixOS built-in `services.btrfs.autoScrub`. If both are enabled, evaluation fails with a clear error. Disable one or the other.

### Monitoring

| Option | Type | Default | Description |
| --- | --- | --- | --- |
| `braid.monitor.enable` | bool | `true` | Enable disk health monitoring |
| `braid.monitor.interval` | string | `"5min"` | Polling interval (systemd time span) |
| `braid.monitor.beep` | bool | `true` | Audible PC speaker beep on alert |
| `braid.monitor.alertCommand` | string or null | `null` | Custom command to run on alert |

When `beep = true`, the module unblacklists the `pcspkr` kernel module, creates a `beep` group, and sets up a udev rule for PC speaker access. The beep loops every 15 seconds until acknowledged with `braid ack`.

`alertCommand` runs in addition to the beep (not instead of). Use it for push notifications, email, etc.:

```nix
braid.monitor.alertCommand = "curl -s -d 'Disk error on NAS' https://ntfy.sh/my-nas-alerts";
```

See [Monitoring and alerts](monitoring-and-alerts.md) for the full workflow.

### Auto-unlock

| Option | Type | Default | Description |
| --- | --- | --- | --- |
| `braid.autoUnlock.enable` | bool | `false` | Enable USB keyfile auto-unlock |
| `braid.autoUnlock.keyDevice` | string | `""` | Block device path (`/dev/disk/by-id/...`) |
| `braid.autoUnlock.timeoutSec` | positive int | `5` | Seconds to wait for USB device |
| `braid.autoUnlock.allowDegraded` | bool | `false` | Mount with missing devices |

`keyDevice` must use a `/dev/disk/by-id/` path -- `/dev/sdX` names shift when devices are added or removed.

The auto-unlock service mounts the USB read-only, reads `braid.key`, unlocks the pool, and unmounts the USB immediately. The keyfile is never left accessible. If the USB is absent at boot, the service exits cleanly without blocking boot.

See [Auto-unlock](auto-unlock.md) for the enrollment and setup workflow.

### Auto-suspend

| Option | Type | Default | Description |
| --- | --- | --- | --- |
| `braid.autoSuspend.enable` | bool | `false` | Suspend NAS when idle |
| `braid.autoSuspend.wolInterface` | string or null | `null` | Network interface for Wake-on-LAN (required) |
| `braid.autoSuspend.idleTime` | positive int | `900` | Seconds idle before suspend |

Requires a wired ethernet interface -- WiFi interfaces are rejected at evaluation time (WoL needs ethtool, which does not work for WiFi).

Activity checks that block suspend:

- `braid idle` -- scrub, balance, or replace in progress
- Active SSH sessions
- Active local sessions (TTY/X11/Wayland)
- SMB connections (auto-detected if `services.samba` is enabled)
- NFS connections (auto-detected if `services.nfs.server` is enabled)

The scrub timer is registered as a wakeup source so the NAS wakes for scheduled scrubs.

See [Power management](power-management.md) for the full workflow.

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

`pwmPath` is found via `pwmconfig`. `minStart` and `maxStop` are measured with `hddfancontrol pwm-test -p <pwm-path>`. All three are hardware-specific.

Monitors all visible SATA devices (not only braid pool members). Requires a board-specific Super I/O driver in `boot.kernelModules` -- see [Fan control](fan-control.md) for the hardware discovery workflow.

## Full config example

Every option with its default (or a representative value for required/optional fields):

```nix
braid = {
  enable = true;
  package = braid.packages.x86_64-linux.default;
  mountPoint = "/mnt/storage";   # default
  storageGroup = "storage";      # default; null to disable

  # Tool version overrides (defaults to nixpkgs versions)
  # packages.cryptsetup = pkgs.cryptsetup;
  # packages.btrfsProgs = pkgs.btrfs-progs;
  # packages.utilLinux = pkgs.util-linux;

  autoScrub = {
    enable = true;       # default
    interval = "monthly"; # default; any systemd calendar expression
  };

  monitor = {
    enable = true;       # default
    interval = "5min";   # default
    beep = true;         # default
    alertCommand = null; # default; e.g. "curl -s -d 'alert' https://ntfy.sh/my-nas"
  };

  autoUnlock = {
    enable = false;  # default
    keyDevice = "/dev/disk/by-id/usb-Kingston_DataTraveler_XXXX-0:0";
    timeoutSec = 5;  # default
    allowDegraded = false; # default
  };

  autoSuspend = {
    enable = false;   # default
    wolInterface = "eno1";
    idleTime = 900;   # default (15 minutes)
  };

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
};
```

## Related

- [Getting started](getting-started.md) -- first-time setup walkthrough
- [Auto-unlock](auto-unlock.md) -- USB keyfile enrollment
- [Monitoring and alerts](monitoring-and-alerts.md) -- alert workflow and custom commands
- [Power management](power-management.md) -- auto-suspend and WoL setup
- [Fan control](fan-control.md) -- hardware discovery and fan control setup
- [Sharing and permissions](sharing-and-permissions.md) -- storage group and Samba
