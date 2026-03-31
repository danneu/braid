# braid

NixOS-native cli tool for managing an encrypted, redundant (raid1) pool of hard drives, especially on a NAS device.

It wraps two standard tools:

- **[luks](https://en.wikipedia.org/wiki/Linux_Unified_Key_Setup)** full disk encryption
- **[btrfs](https://btrfs.readthedocs.io/en/latest/)** file system for native raid1 redundancy, checksumming, self-healing

## Features

- Full disk encryption - plug in a USB keyfile or run `braid unlock` with a passphrase to mount
- Redundancy - data is stored on two disks, so you can always tolerate a single disk failure
- Dynamic pool — add or remove drives incrementally
- Self-healing data - btrfs checksums and silently repairs corruption from the redundant copy
- CLI-owned membership - add drives with `braid add`, no `nixos-rebuild` required
- Dashboard - `braid tui` shows you the state of your system

## Downsides

- RAID1
  - simple, but it means you lose half of your capacity to redundancy; four 12TB drives will only give you 48TB/2 = 24TB storage
  - more drives won't increase your redundacy
  - only tolerates one disk failure at a time
- HDD-first
  - defaults are tuned for spinning drives (no TRIM/discard passthrough, HDD-oriented scrub scheduling)
  - flash media (SSDs, NVMe, USB sticks) may work but are not validated or optimized

## Goals and aspirations

- Simple - I want anyone to be able to use this
- Well-tested - Every bug, fix, and regression I encounter is turned into another NixOS VM test in `tests/`

## Hardware notes

braid works on any NixOS x86_64 machine. A few component choices matter for compatibility:

- **10GbE NIC** — Intel X540 (`ixgbe` driver) has rock-solid Linux support and reliable Wake-on-LAN. Avoid Aquantia/Marvell AQC107 (`atlantic` driver) if WoL matters — it's firmware-dependent and hit-or-miss on Linux.

## Install

Add braid to your flake inputs and import the module:

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

## Example

```nix
# NixOS config — just enable the module
braid = {
  enable = true;
  mountPoint = "/mnt/storage";  # default
};
```

```sh
# Add drives — no nixos-rebuild needed
sudo braid add toshiba=/dev/disk/by-id/ata-Toshiba_MN07_XXXX \
               ironwolf=/dev/disk/by-id/ata-Ironwolf_ST12_YYYY
```

Each disk gets a human-friendly name used in CLI commands, systemd mapper names (`braid-toshiba`), logs, and error messages. Disk membership is stored in `/var/lib/braid/pool.json`, owned by the CLI.

## Managing drives

### Find your disks

```
$ lsblk -d -o NAME,SIZE,ID-LINK
NAME      SIZE ID-LINK
sda      12.0T ata-Toshiba_MN07ACA12TEA_XXXXXXXXXXXX
sdb      12.0T ata-Ironwolf_ST12000VN0008_YYYYYYYY
```

Use the ID-LINK column to build your by-id paths.

### Start with multiple disks (recommended)

```sh
sudo braid add toshiba=/dev/disk/by-id/ata-Toshiba_MN07_XXXX \
               ironwolf=/dev/disk/by-id/ata-Ironwolf_ST12_YYYY
```

`braid add` asks for a passphrase once, LUKS-formats both disks, and creates a btrfs RAID1 pool directly — no balance needed. The pool is live immediately at `/mnt/storage` with full redundancy.

### Start with one disk

```sh
sudo braid add toshiba=/dev/disk/by-id/ata-Toshiba_MN07_XXXX
```

The pool is live at `/mnt/storage` but with no redundancy — data is available but unprotected until a second drive is added.

### Add a drive

```sh
sudo braid add ironwolf=/dev/disk/by-id/ata-Ironwolf_ST12_YYYY
```

The pool converts to RAID1 automatically. Existing data rebalances to the new disk in the background. You can also add multiple disks at once:

```sh
sudo braid add ironwolf=/dev/disk/by-id/ata-Ironwolf_ST12_YYYY \
               seagate=/dev/disk/by-id/ata-Seagate_ST12_ZZZZ
```

`braid add` handles three cases:

- **Fresh disk** (no LUKS header) — LUKS-formats, opens, and adds to pool.
- **Returning braid disk** (braid-labeled LUKS, btrfs FSID matches current pool) — identity-verified recovery add.
- **Refused** — non-braid LUKS, braid disk from a different pool, braid-labeled but no btrfs superblock (ambiguous state — wipe the disk and retry), or existing LUKS when the pool is not mounted (bootstrap only accepts fresh disks).

### Preview before executing

```sh
$ sudo braid add toshiba=/dev/disk/by-id/ata-Toshiba_MN07_XXXX \
                 ironwolf=/dev/disk/by-id/ata-Ironwolf_ST12_YYYY --dry-run
[destructive] LUKS format /dev/disk/by-id/ata-Toshiba_MN07_XXXX
[safe       ] LUKS open → braid-toshiba
[destructive] LUKS format /dev/disk/by-id/ata-Ironwolf_ST12_YYYY
[safe       ] LUKS open → braid-ironwolf
[safe       ] mkfs.btrfs RAID1 /dev/mapper/braid-toshiba /dev/mapper/braid-ironwolf
[safe       ] mount → /mnt/storage
```

Steps reflect actual disk state — if a disk is already LUKS-formatted, the destructive step is omitted.

### Remove a drive

```
sudo braid remove ironwolf
```

Data migrates off the drive before it's detached. If removing would leave a single disk (losing redundancy), confirmation is required. The disk is removed from pool membership (`pool.json`) automatically.

### Remove a missing/dead device (cleanup only)

Forgets a stale missing-device entry from the pool. This does **not** rebuild data — use `braid replace` for that. When clearing the last missing device with ≥2 survivors, automatically runs a soft RAID1 balance to restore redundancy for chunks written during degraded operation.

```
sudo braid remove-missing                    # 1 missing device: auto-detected
sudo braid remove-missing --missing-id 3     # multiple missing: target by devid
```

Use `braid status` to see device IDs.

### Replace a drive

Replace works for both live and dead/missing disks using `btrfs replace start`. The new device inherits the old device's slot — no intermediate balance or remove step.

**Live disk** (swap a working drive):

```
sudo braid replace --old ironwolf --new seagate=/dev/disk/by-id/ata-Seagate_NEW_ZZZZ
```

**Dead/missing disk** (after a drive failure):

```
sudo braid replace --old ironwolf --new seagate=/dev/disk/by-id/ata-Seagate_NEW_ZZZZ
sudo braid replace --old ironwolf --new seagate=/dev/disk/by-id/ata-Seagate_NEW_ZZZZ --missing-id 3   # explicit devid when multiple missing
```

Use `braid status` to see device IDs. If the pool has missing devices when you try a live replace, repair the missing device first with `braid replace --missing-id <devid>`. Use `braid remove-missing` only to intentionally forget a stale entry without rebuilding data. When replacing a missing device and clearing the last missing entry with ≥2 devices remaining, a soft RAID1 balance runs automatically to restore redundancy.

### Discover pool members

If `pool.json` is lost or corrupt, `braid discover` scans `/dev/disk/by-id/` for LUKS devices with `braid-*` labels and reconstructs membership:

```sh
sudo braid discover              # show what's found
sudo braid discover --write      # persist to pool.json
```

This is a repair tool — the normal path to create `pool.json` is `braid add`.

### Disk identity map

Braid maintains state files in `/var/lib/braid/`:

- **`pool.json`** — authoritative disk membership with enriched metadata. Maps disk names to `/dev/disk/by-id/` paths plus `luks_uuid`, `devid`, and `added_at`. Written by `braid add`, `remove`, `replace`, `remove-missing`, `discover --write`, and `recover`. Metadata fields enriched by `unlock` on each mount. If missing or corrupt, `unlock` fails with a clear error directing you to `braid discover --write`.
- **`pending-op.json`** — pending-operation journal (transient). Present only during mutations. When present, braid enters recovery mode — only `status`, `recover`, and `lock` are allowed. `braid recover --passphrase-stdin` opens LUKS, mounts the pool, rebuilds membership from live state, and clears the journal. If devices are missing, pass `--allow-degraded`.

Disk names are immutable once assigned. Renaming/reassigning a name is rejected by mutating commands. Keep the original name, or use explicit `braid replace` / `braid remove` + `braid add` workflows.

### Pool status

```sh
sudo braid status             # pool health + per-disk detail
sudo braid status --json      # machine-readable output
```

Drive states:

- `present` — disk is in the pool and online
- `missing` — in pool membership but device is absent (unplugged, failed, powered off)

Pool status values (`--json` `"status"` field / human output):

- `"intact"` / `intact` — mounted, all devices present
- `"degraded"` / `DEGRADED (N missing device(s))` — mounted, one or more devices missing
- `"not_mounted"` / `not mounted` — pool is not mounted

Human output includes a per-type allocation table:

```
Allocation:
  Type       Profile  Used        Allocated
  Data       RAID1    153.40 GiB  157.00 GiB
  Metadata   RAID1    156.50 MiB  1.00 GiB
  System     RAID1    0.00 MiB    32.00 MiB
```

The `--json` output includes an `"allocation"` array with `bg_type`, `profile`, `used_bytes`, and `allocated_bytes` per entry.

During an active balance (e.g. after `braid add`), status shows progress:

```
Balance:  running, 108/160 chunks (68% complete)
```

The `--json` output includes a `"balance"` object with `"state"` (`running`, `paused`, `idle`, or `unknown`) and chunk progress fields.

### Diagnostics

```sh
sudo braid doctor           # check config, pool health, profile consistency
sudo braid doctor --json    # machine-readable output
```

### Browse btrfs commands

Interactive read-only browser for raw btrfs command output:

```sh
sudo braid browse
```

Tabs: Filesystem, Devices, Subvolumes, Scrub, Balance. Each tab runs the corresponding `btrfs` command and dumps the output. Press `r` to reload, `Tab`/`Shift-Tab` to switch tabs, `h`/`l` for subtabs, `j`/`k` to scroll. In the Subvolumes tab, `Enter` drills into a selected subvolume's detail.

```sh
sudo braid browse --check    # non-interactive: verify all browse commands succeed
```

### Non-interactive mode

For scripting, use `--passphrase-stdin` or `--passphrase-file` with `--yes`:

```sh
echo 'secret' | sudo braid add ironwolf=/dev/disk/by-id/ata-Ironwolf_ST12_YYYY --passphrase-stdin --yes
sudo braid add ironwolf=/dev/disk/by-id/ata-Ironwolf_ST12_YYYY --yes --passphrase-file /run/secrets/luks
```

## Pool unlock

After boot, bring the encrypted pool online:

```sh
systemctl start braid-pool.target
```

One passphrase prompt opens all available LUKS devices and mounts the pool. Works from TTY, SSH, or scripted. If disks are missing, use `--allow-degraded` to mount with reduced redundancy.

When unlocking on a fresh system (e.g., after migrating disks to a new machine), `unlock` automatically rebuilds the disk identity map from live pool state. Each disk's on-disk LUKS label is verified before recording.

braid always mounts the top-level subvolume explicitly (`subvolid=5`), so `btrfs subvolume set-default` changes can't alter what gets mounted.

Interrupted balance operations (e.g., from a crash or `braid lock` during rebalance) are **never silently resumed** on unlock. braid mounts with `skip_balance` and warns if a paused balance is detected:

```
[warn]  paused balance detected — will not auto-resume
           resume:  btrfs balance resume /mnt/storage
           cancel:  btrfs balance cancel /mnt/storage
```

## Auto-unlock with USB keyfile

For unattended reboots, a binary random keyfile on a removable USB device can auto-unlock the pool without typing a passphrase.

### Generate and enroll a keyfile

```sh
sudo braid enroll /mnt/usb --generate
```

This creates a 4096-byte random keyfile at `/mnt/usb/braid.key` (mode 400) and enrolls it into LUKS slot 1 on all pool disks in one step. Slot conflicts and passphrase are validated before the keyfile is created.

### Enroll an existing keyfile

When your usb stick already has a braid.key on it:

```sh
sudo braid enroll /mnt/usb
```

Enrolls an existing `braid.key` in the given directory into all pool disks. The passphrase (slot 0) still works.

### LUKS header backups

`braid add`, `braid replace`, and `braid enroll` automatically back up LUKS headers to `/var/lib/braid/luks-headers/<mapper>.luksheader` after formatting or enrolling keyfiles.

`braid status` and `braid tui` warn when local backups exist — copy them to encrypted offline media (e.g. USB drive in a safe) and delete the local copies. These files contain sensitive keyslot metadata needed for recovery and should not be left on an unencrypted boot drive.

### Enroll during `braid add`

```sh
sudo braid add ironwolf=/dev/disk/by-id/ata-Ironwolf_ST12_YYYY --enroll /mnt/usb
```

### Enable auto-unlock during boot

```nix
braid.autoUnlock = {
  enable = true;
  keyDevice = "/dev/disk/by-id/usb-Kingston_DataTraveler_XXXX-0:0";
  timeoutSec = 5;          # seconds to wait for USB (default)
};
```

```sh
sudo nixos-rebuild switch
```

On boot, the `braid-auto-unlock` service mounts the USB read-only, unlocks the pool with the keyfile, then unmounts the USB. If the USB is missing or the keyfile is wrong, boot continues normally with the pool locked — unlock manually with `braid unlock` or `systemctl start braid-pool.target`.

**Note:** `braid-pool.target` does not reflect auto-unlock state. Check mount state with `mountpoint -q /mnt/storage`.

**Security:** For maximum security, remove the USB key after the pool unlocks. If the USB remains in the server, an attacker who steals both the server and USB can unlock all drives — the encryption provides no protection against physical theft of the combined unit.

## Shell Completions

Tab completion for subcommands, flags, and disk names works out of the box on NixOS when `braid.enable = true`. Completions are registered for bash, zsh, and fish.

```sh
braid <TAB>              # → add  remove  remove-missing  replace  status  doctor
braid remove <TAB>       # → toshiba  ironwolf  seagate
braid add --<TAB>        # → --dry-run  --yes  --passphrase-file  --progress
```

Disk name candidates are read from `/var/lib/braid/pool.json` on every tab press, so they reflect your current pool membership.

## What you get for free

Braid enables these automatically when `braid.enable = true`:

- **Monthly btrfs scrub** — detects and repairs bit rot before it can compound. Override or disable with normal NixOS config:

  ```nix
  # change to weekly
  services.btrfs.autoScrub.interval = "weekly";

  # or disable
  services.btrfs.autoScrub.enable = false;
  ```

- **Resilient boot** — a dead or missing drive never blocks boot. The pool stays locked until you unlock it, and the system remains reachable via SSH.

- **Pinned toolchain** — runtime tools (btrfs-progs, cryptsetup, util-linux) are pinned to a NixOS stable release. Parser output formats don't change on flake updates. Override individual tools via `braid.packages.*` if needed.

### Mount Point Permissions

braid sets the mount root to `root:storage 2770` after mount-producing commands
(`unlock`, `add`). Users in the `storage` group can read and write the mount root directory.
New entries inherit the `storage` group via setgid.

Note: individual file permissions still depend on the creating process's umask.
For collaborative access, ensure users set `umask 002` or configure Samba with
`force create mode` / `force directory mode`.

Add a user to the storage group:

```nix
users.users.myuser.extraGroups = [ config.braid.storageGroup ];
```

Customize the group name:

```nix
braid.storageGroup = "nas-users";
```

Disable automatic permissions:

```nix
braid.storageGroup = null;
```

## Samba

Samba is not part of the braid module — NixOS already provides declarative Samba config. Reference `config.braid.mountPoint` to stay in sync:

```nix
services.samba = {
  enable = true;
  openFirewall = true;
  settings = {
    videos = {
      path = "${config.braid.mountPoint}/videos";
      "guest ok" = "yes";
      "read only" = "yes";
      browseable = "yes";
    };
  };
};
```

## Monitoring and Alerts

braid has first-class alerts for disk health. When something is wrong — a missing device, btrfs errors, or a SMART warning — braid detects it and alerts you via the motherboard speaker (enabled by default, disable with `beep = false`).

**How it works:**

- `braid monitor` checks btrfs device stats, missing devices, and SMART alerts. Exits 0 (healthy or pool offline), 1 (alert active), or 2 (monitor error).
- A systemd timer runs `braid monitor` every 5 minutes (configurable). On exit 1, it starts the alert service.
- `braid status` shows an ALERT banner with cause details when an alert is active.
- `braid ack` acknowledges the alert, silences the beeper, clears alerts from status and TUI, and sets a baseline so the same condition won't re-trigger.

**Enabled by default.** When `braid.enable = true`, monitoring is active. To disable:

```nix
braid.monitor.enable = false;
```

**Configuration:**

```nix
braid.monitor = {
  interval = "5min";       # polling interval (systemd time span)
  beep = true;             # motherboard speaker alert (set false to disable)
  alertCommand = null;     # optional command to run on alert (runs as root)
};
```

**Example workflow:**

```bash
# NAS beeps — SSH in
sudo braid status
# ALERT -- disk health issue detected. Run 'braid ack' to acknowledge and silence.
#   - missing device (devid 2)

# Replace the failed disk:
sudo braid replace --old bad-disk --new new-disk=/dev/disk/by-id/ata-NEW_DISK

# Acknowledge the alert (stops beeping)
sudo braid ack
```

## Auto-Suspend

braid can automatically suspend the NAS when idle and wake it on demand via Wake-on-LAN or for scheduled maintenance (monthly scrub).

Uses [autosuspend](https://github.com/languitar/autosuspend) under the hood. braid configures it with the right checks for a NAS: btrfs operations, SSH sessions, SMB/NFS connections.

```nix
braid.autoSuspend = {
  enable = true;
  wolInterface = "eno1";  # your primary wired NIC (find with: ip link)
};
```

**What it does:**

- Suspends after 15 minutes of idle (configurable with `braid.autoSuspend.idleTime`)
- Blocks suspend during: btrfs scrub, balance, replace, active SSH sessions, local interactive sessions (TTY/X11/Wayland)
- Auto-detects SMB clients (if `services.samba` enabled) and NFS clients (if `services.nfs.server` enabled)
- Wakes the machine via RTC alarm for the monthly btrfs scrub timer
- smartd and braid-monitor run opportunistically during wake windows

**Prerequisites:** Wake-on-LAN must be working — see [Wake-on-LAN troubleshooting](#wake-on-lan-troubleshooting) below.

**Configuration:**

```nix
braid.autoSuspend = {
  enable = true;
  wolInterface = "eno1";  # required — find with: ip link
  idleTime = 900;         # seconds before suspend (default: 15 min)
};
```

For additional idle checks beyond what braid configures, use `services.autosuspend.checks` directly.

### Wake-on-LAN troubleshooting

Wake-on-LAN lets clients wake a suspended NAS by sending a magic packet. braid's auto-suspend feature relies on it.

WoL on Linux can require some trial and error due to differences and bugs between motherboards. For reference, my Gigabyte B550I AORUS PRO AX with an RTL8125 NIC needed all three fixes below: spurious ACPI wake sources, the vendor NIC driver, and PCI bridge wakeup.

**BIOS:** Enable Wake on LAN and disable ErP (ErP cuts standby power to the NIC during sleep).

**Test basic suspend first.** Many motherboards have ACPI wake sources that cause instant resume from suspend:

```sh
sudo systemctl suspend   # does it stay suspended?
```

If the system wakes immediately, find the culprit by disabling ACPI wake sources:

```sh
$ cat /proc/acpi/wakeup
Device  S-state   Status   Sysfs node
GP12      S4    *enabled   pci:0000:00:07.1
XHC0      S4    *enabled   pci:0000:09:00.3     # ← USB controller
GPP0      S4    *enabled   pci:0000:00:01.1     # ← PCIe bridge
...

$ echo XHC0 | sudo tee /proc/acpi/wakeup       # toggle one off
$ sudo systemctl suspend                        # test again
```

Binary search until you find which source(s) cause the spurious wake. Common offenders: USB controllers (XHC, PTXH), PCIe bridges (GPP0). Once identified, disable them at boot:

```nix
# Example: XHC0 and GPP0 were causing instant wake on a B550I AORUS PRO AX.
systemd.services.disable-spurious-wakeup = {
  description = "Disable ACPI wake sources that cause spurious resume";
  wantedBy = [ "multi-user.target" ];
  serviceConfig = {
    Type = "oneshot";
    ExecStart = "${pkgs.bash}/bin/bash -c 'echo XHC0 > /proc/acpi/wakeup; echo GPP0 > /proc/acpi/wakeup'";
  };
};
```

**Test WoL.** From another machine on the same LAN:

```sh
# on the NAS — find MAC address and confirm WoL is enabled
$ ip -brief link show
lo               UNKNOWN        00:00:00:00:00:00
eno1             UP             18:c0:4d:3e:88:07   # ← MAC address

$ sudo ethtool eno1 | grep Wake-on
        Supports Wake-on: pumbg
        Wake-on: g                                   # ← "g" = magic packet, good

# suspend the NAS
$ sudo systemctl suspend

# from a client (e.g. macOS)
$ wakeonlan 18:c0:4d:3e:88:07   # brew install wakeonlan
```

**NIC driver.** If the NIC link light goes off during suspend and WoL doesn't work, the kernel driver may not be keeping the NIC powered. RTL8125 NICs are known to need the vendor `r8125` driver instead of the kernel's `r8169`:

```nix
boot.extraModulePackages = [ config.boot.kernelPackages.r8125 ];
boot.blacklistedKernelModules = [ "r8169" ];
```

**PCI bridge wakeup.** WoL magic packets generate a PME (Power Management Event) that must propagate from the NIC through every PCI bridge to the CPU. If any bridge in the chain has wakeup disabled, the signal is silently dropped. Find your NIC's bridge chain and check:

```sh
$ readlink -f /sys/class/net/eno1/device
/sys/devices/pci0000:00/0000:00:01.2/0000:02:00.2/0000:03:08.0/0000:05:00.0
#                       ^^^^^^^^^^^^  ^^^^^^^^^^^^  ^^^^^^^^^^^^  ^^^^^^^^^^^^
#                       bridge        bridge        bridge        NIC

# check each bridge in the path (exclude the NIC itself)
$ for p in /sys/devices/pci0000:00/0000:00:01.2/power/wakeup \
           /sys/devices/pci0000:00/0000:00:01.2/0000:02:00.2/power/wakeup \
           /sys/devices/pci0000:00/0000:00:01.2/0000:02:00.2/0000:03:08.0/power/wakeup; do
    echo "$p: $(cat $p)"
  done
/sys/.../0000:00:01.2/power/wakeup: disabled     # ← problem
/sys/.../0000:02:00.2/power/wakeup: disabled     # ← problem
/sys/.../0000:03:08.0/power/wakeup: enabled      # ok
```

Any bridge showing `disabled` needs a udev rule to persist across reboots:

```nix
# Enable wakeup on PCI bridges so WoL magic packets propagate to the CPU.
# Addresses are hardware-specific — find yours with: readlink -f /sys/class/net/<iface>/device
services.udev.extraRules = ''
  ACTION=="add", SUBSYSTEM=="pci", KERNEL=="0000:00:01.2", ATTR{power/wakeup}="enabled"
  ACTION=="add", SUBSYSTEM=="pci", KERNEL=="0000:02:00.2", ATTR{power/wakeup}="enabled"
'';
```

## Usage/NAS recommendations

### Create btrfs subvolumes for different silos of data

Btrfs subvolumes are like directories that can be individually snapshotted and restored.

Naive:

```sh
mkdir /mnt/storage/movies
mkdir /mnt/storage/my-poetry
```

With subvolumes:

```sh
btrfs subvolume create /mnt/storage/movies
btrfs subvolume create /mnt/storage/my-poetry
```

The subvolume approach lets you do things like snapshot and restore `my-poetry` without touching `movies`, back up just `my-poetry` offsite, or run different snapshot schedules for each (e.g. hourly for poetry, weekly for movies).

Subvolumes look and act like regular directories — you read and write files the same way. The only difference is that btrfs tracks them independently, so you get granular control over snapshots, backups, and rollbacks.

There's no cost to creating subvolumes upfront. They share the same disk space with no pre-allocation. It's much harder to convert a regular directory into a subvolume later, so prefer creating subvolumes from the start for any top-level data category you care about.

## Development

Braid is developed test-first with NixOS VM tests.

Typical loop:

```bash
# run one test while iterating
just test braid-add-disk

# run a few specific tests
just test braid-add-disk braid-remove-disk

# add verbose VM logs
just test braid-add-disk -v

# run full suite before finishing
just test
```

### Faster tests with tmpfs

#### NixOS

VM tests create qcow2 disk images that hammer your SSD. Mount a dedicated tmpfs so builds happen in RAM:

```nix
# NixOS config (e.g. hosts/silverstone/configuration.nix)
fileSystems."/tmp-braid" = {
  device = "tmpfs";
  fsType = "tmpfs";
  options = [ "size=16G" "mode=0755" ];
};
```

Then pass `--option build-dir /tmp-braid` to nix commands, or use `just test` / `just test-fast` which do this automatically.

Rust CLI code lives in `cli/`. Build it directly with:

```bash
nix build .#braid
```
