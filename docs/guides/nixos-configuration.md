[← braid](../index.md)

# NixOS configuration

Complete reference for the braid NixOS module options. Read this when setting up braid for the first time or tuning behavior after initial setup.

## Minimal config

```nix
# flake.nix
{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
    braid.url = "github:danneu/braid?ref=release";
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

`?ref=release` pins braid to its release channel: a moving branch the release
fast-forwards to each tag. `nix flake update braid` is then the "upgrade to the
newest release" button, and `flake.lock` still pins the exact rev. The snippet
deliberately omits `braid.inputs.nixpkgs.follows` -- see [Binary cache](#binary-cache)
and [Tool overrides](#tool-overrides) for why no-follows is the default.

The `braid.url` input takes any of:

```nix
# flake.nix
braid.url = "github:danneu/braid?ref=release";   # newest release (default)
braid.url = "github:danneu/braid?ref=v0.0.1";    # pin a tag
braid.url = "github:danneu/braid?rev=<commit>";  # pin an exact commit
```

```nix
# configuration.nix
braid = {
  enable = true;
};
```

`nixosModules.default` supplies `braid.package` automatically. Override it only
to build the CLI yourself.

## Binary cache

braid publishes a prebuilt `x86_64-linux` CLI to a public Cachix cache on every
release. Add it so the NAS pulls the binary instead of recompiling Rust:

```nix
# configuration.nix
nix.settings = {
  extra-substituters = [ "https://braid.cachix.org" ];
  extra-trusted-public-keys = [ "braid.cachix.org-1:I/p7fx1z5n0+O80KzMuT7aXRdkVyHr/buZKaBu7HvJs=" ];
};
```

The cache only hits when braid resolves to the exact store path CI built -- that
is, with the recommended no-`follows` setup above. Setting
`braid.inputs.nixpkgs.follows` rebuilds braid against your nixpkgs, producing a
different path and a cache miss. See [Toolchain pinning](../design/decisions/010-toolchain-pinning.md)
and [ADR 029](../design/decisions/029-release-process.md).

## What you get for free

When `braid.enable = true`, the module sets up:

- **Monthly btrfs scrub** -- timer + service tied to pool lifecycle. Configurable via `braid.autoScrub`.
- **Resilient boot** -- a dead drive never blocks boot. LUKS open and btrfs mount are deferred to `braid unlock`, not wired into `boot.initrd`.
- **Pinned toolchain** -- btrfs-progs, cryptsetup, NUT, smartmontools, and ethtool are pinned to NixOS stable versions. util-linux and systemd are host-provided stable contracts. Override package-backed tools with `braid.packages.*` if needed.
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
| `braid.package` | package or null | `null` | The braid CLI package; `nixosModules.default` defaults it to `braid-cli-unwrapped` |
| `braid.mountPoint` | path | `/mnt/storage` | Canonical absolute path, with no whitespace or shell metacharacters |
| `braid.poolAccessGroup` | string or null | `"storage"` | Group for mount point access. `null` to disable |
| `braid.lockSystemdStopDeadlineSecs` | positive int | `270` | Seconds to wait for the pool lock during `braid-online.service` ExecStop; must stay below the unit's `TimeoutStopSec` |

### Tool overrides

| Option | Type | Default | Description |
| --- | --- | --- | --- |
| `braid.packages.cryptsetup` | package | `pkgs.cryptsetup` | cryptsetup package |
| `braid.packages.btrfsProgs` | package | `pkgs.btrfs-progs` | btrfs-progs package |
| `braid.packages.utilLinux` | package | `pkgs.util-linux` | util-linux package |
| `braid.packages.nut` | package | `pkgs.nut` | NUT package |
| `braid.packages.smartmontools` | package | `pkgs.smartmontools` | smartmontools package |
| `braid.packages.ethtool` | package | `pkgs.ethtool` | ethtool package |

Override these only if you need a specific version for compatibility testing. The recommended setup omits `braid.inputs.nixpkgs.follows` (the Minimal config example above), so `nixosModules.default` sources the five pinned tool defaults from braid's own pinned `nixpkgs` (nixos-26.05) -- the same versions the release binary cache is built against, so braid is a cache hit. util-linux defaults to your system's `pkgs.util-linux` either way. Adding `braid.inputs.nixpkgs.follows = "nixpkgs"` is an advanced opt-out: it dedups your closure but rebuilds braid against your nixpkgs (a cache miss) and sources the five pinned tools from your nixpkgs instead. If you take it, keep your nixpkgs on the same NixOS stable release braid targets so the parsed tool output stays compatible -- see [Toolchain pinning](../design/decisions/010-toolchain-pinning.md).

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
| `braid.monitor.alertCommandTimeoutSec` | positive int | `60` | Seconds before braid stops a custom alert command |

When `beep = true`, the module unblacklists the `pcspkr` kernel module, creates a `beep` group, and sets up a udev rule for PC speaker access. The beep loops with exponential backoff (5s, 10s, 20s, 40s, ...) capped at once every 15 minutes, until acknowledged with `braid ack`.

`alertCommand` runs in addition to the beep (not instead of). Use it for push notifications, email, etc.:

```nix
braid.monitor.alertCommand = "curl -s -d 'Disk error on NAS' https://ntfy.sh/my-nas-alerts";
```

The command runs as root and is bounded by
`braid.monitor.alertCommandTimeoutSec` (default 60 seconds) on both Critical
and Warning alert paths.

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

- `braid idle` -- scrub or any btrfs kernel exclusive operation (balance, device add/remove/replace, resize, swap activate)
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
| `braid.fanControl.pwm.platformDevice` | string | (required) | Platform device name under `/sys/devices/platform/` |
| `braid.fanControl.pwm.number` | int | (required) | PWM channel number (1-based) |
| `braid.fanControl.pwm.minStart` | int | (required) | Minimum PWM to start fan from standstill |
| `braid.fanControl.pwm.maxStop` | int | (required) | PWM below which a spinning fan stalls |
| `braid.fanControl.minTemp` | int | `30` | Temperature (C) at which fan runs at minimum speed |
| `braid.fanControl.maxTemp` | int | `40` | Temperature (C) at which fan runs at full speed |
| `braid.fanControl.minFanSpeedPercent` | int | `20` | Minimum fan speed % (0 = fan may stop) |
| `braid.fanControl.interval` | string | `"30s"` | Temperature polling interval |

`pwm.platformDevice` and `pwm.number` are found via `pwmconfig`. `pwm.minStart` and `pwm.maxStop` are measured with `hddfancontrol pwm-test -p <pwm-path>`. All four are hardware-specific.

Monitors all visible SATA devices (not only braid pool members). Requires a board-specific Super I/O driver in `boot.kernelModules` -- see [Fan control](fan-control.md) for the hardware discovery workflow.

### UPS

| Option | Type | Default | Description |
| --- | --- | --- | --- |
| `braid.ups.enable` | bool | `false` | Enable UPS support via NUT (single-host standalone) |
| `braid.ups.name` | string | `"ups"` | UPS identifier for `upsd`/`upsc` |
| `braid.ups.driver` | string | `"usbhid-ups"` | NUT driver; the USB default covers most home-NAS UPSes |
| `braid.ups.port` | string | `"auto"` | Driver port; `auto` finds the first matching USB UPS |

When enabled, NUT triggers an orderly poweroff on low battery (unwinding `braid-online.service` -> `braid lock` -> unmount) and pool-mutating commands (`add`/`remove`/`remove-missing`/`replace`) refuse to start unless the UPS reports verified utility power (`OL`). Only `name` is written to `/etc/braid/config.json`, so `braid ups status` and the TUI know which UPS to query; `driver` and `port` configure the NUT driver only. Non-USB drivers (`apcsmart`, `snmp-ups`) are an escape hatch and not first-class.

See [UPS](ups.md) for the setup workflow and live status.

## Full config example

Every option with its default (or a representative value for required/optional fields):

```nix
braid = {
  enable = true;
  # package -- defaults to nixosModules.default's pinned braid-cli-unwrapped;
  # set only to build the CLI yourself.
  mountPoint = "/mnt/storage";   # default
  poolAccessGroup = "storage";   # default; null to disable
  lockSystemdStopDeadlineSecs = 270;  # default; must stay below braid-online TimeoutStopSec

  # Tool version overrides -- the recommended setup omits nixpkgs `follows`, so
  # five pinned defaults come from braid's pinned nixos-26.05 (cache hit), while
  # util-linux comes from your pkgs. `follows` tracks your nixpkgs for the pinned
  # tools but rebuilds braid (cache miss). See "Tool overrides".
  # packages.cryptsetup = pkgs.cryptsetup;
  # packages.btrfsProgs = pkgs.btrfs-progs;
  # packages.utilLinux = pkgs.util-linux;
  # packages.nut = pkgs.nut;
  # packages.smartmontools = pkgs.smartmontools;
  # packages.ethtool = pkgs.ethtool;

  autoScrub = {
    enable = true;       # default
    interval = "monthly"; # default; any systemd calendar expression
  };

  monitor = {
    enable = true;       # default
    interval = "5min";   # default
    beep = true;         # default
    alertCommand = null; # default; e.g. "curl -s -d 'alert' https://ntfy.sh/my-nas"
    alertCommandTimeoutSec = 60; # default
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
    pwm = {
      platformDevice = "f71882fg.656";  # from pwmconfig (required)
      number = 2;                        # from pwmconfig (required)
      minStart = 65;     # from hddfancontrol pwm-test (required)
      maxStop = 60;      # from hddfancontrol pwm-test (required)
    };
    minTemp = 30;      # default
    maxTemp = 40;      # default
    minFanSpeedPercent = 20;  # default
    interval = "30s";  # default
  };

  ups = {
    enable = false;        # default; opt-in
    name = "ups";          # default
    driver = "usbhid-ups"; # default
    port = "auto";         # default
  };
};
```

## Related

- [Getting started](getting-started.md) -- first-time setup walkthrough
- [Auto-unlock](auto-unlock.md) -- USB keyfile enrollment
- [Monitoring and alerts](monitoring-and-alerts.md) -- alert workflow and custom commands
- [Power management](power-management.md) -- auto-suspend and WoL setup
- [UPS](ups.md) -- NUT-backed orderly poweroff, preflight safety, live status
- [Fan control](fan-control.md) -- hardware discovery and fan control setup
- [Sharing and permissions](sharing-and-permissions.md) -- storage group and Samba
