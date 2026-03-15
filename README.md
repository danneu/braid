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
- Declarative config - declare your disks in nix; the pool state follows the config
- Dashboard - `braid tui` shows you the state of your system

## Downsides

- RAID1
  - simple, but it means you lose half of your capacity to redundancy; four 12TB drives will only give you 48TB/2 = 24TB storage
  - more drives won't increase your redundacy
  - only tolerates one disk failure at a time

## Goals and aspirations

- Simple - I want anyone to be able to use this
- Well-tested - Every bug, fix, and regression I encounter is turned into another NixOS VM test in `tests/`

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
braid = {
  enable = true;
  disks = {
    toshiba  = { byId = "/dev/disk/by-id/ata-Toshiba_MN07_XXXX"; };
    ironwolf = { byId = "/dev/disk/by-id/ata-Ironwolf_ST12_YYYY"; };
  };
  mountPoint = "/mnt/storage";  # default
};
```

Each disk gets a human-friendly name used in CLI commands, systemd mapper names (`braid-toshiba`), logs, and error messages.

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

```nix
braid = {
  enable = true;
  disks = {
    toshiba  = { byId = "/dev/disk/by-id/ata-Toshiba_MN07_XXXX"; };
    ironwolf = { byId = "/dev/disk/by-id/ata-Ironwolf_ST12_YYYY"; };
  };
};
```

```sh
sudo nixos-rebuild switch
sudo braid add toshiba ironwolf
```

`braid add` asks for a passphrase once, LUKS-formats both disks, and creates a btrfs RAID1 pool directly — no balance needed. The pool is live immediately at `/mnt/storage` with full redundancy.

### Start with one disk

```sh
sudo braid add toshiba
```

The pool is live at `/mnt/storage` but with no redundancy — data is available but unprotected until a second drive is added.

### Add a drive

```nix
# Add to braid.disks:
braid.disks.ironwolf = { byId = "/dev/disk/by-id/ata-Ironwolf_ST12_YYYY"; };
```

```
sudo nixos-rebuild switch
sudo braid add ironwolf
```

The pool converts to RAID1 automatically. Existing data rebalances to the new disk in the background. You can also add multiple disks at once: `sudo braid add ironwolf seagate`.

`braid add` handles three cases:

- **Fresh disk** (no LUKS header) — LUKS-formats, opens, and adds to pool.
- **Returning braid disk** (braid-labeled LUKS, btrfs FSID matches current pool) — identity-verified recovery add.
- **Refused** — non-braid LUKS, braid disk from a different pool, braid-labeled but no btrfs superblock (ambiguous state — wipe the disk and retry), or existing LUKS when the pool is not mounted (bootstrap only accepts fresh disks).

### Preview before executing

```sh
$ sudo braid add toshiba ironwolf --dry-run
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

Data migrates off the drive before it's detached. If removing would leave a single disk (losing redundancy), confirmation is required. After removing, update config and rebuild:

```nix
# Remove from braid.disks, then:
sudo nixos-rebuild switch
```

### Remove a missing/dead device (cleanup only)

Forgets a stale missing-device entry from the pool. This does **not** rebuild data — use `braid replace` for that. When clearing the last missing device with ≥2 survivors, automatically runs a soft RAID1 balance to restore redundancy for chunks written during degraded operation.

```
sudo braid remove-missing                    # 1 missing device: auto-detected
sudo braid remove-missing --missing-id 3     # multiple missing: target by devid
```

Use `braid status --verbose` to see device IDs.

### Replace a drive

Replace works for both live and dead/missing disks using `btrfs replace start`. The new device inherits the old device's slot — no intermediate balance or remove step.

**Live disk** (swap a working drive):

```
sudo braid replace --old ironwolf --new seagate
```

**Dead/missing disk** (after a drive failure):

```nix
# Add replacement to config:
braid.disks.seagate = { byId = "/dev/disk/by-id/ata-Seagate_NEW_ZZZZ"; };
# (keep dead disk in config until replace completes, then remove it)
```

```
sudo nixos-rebuild switch
sudo braid replace --old ironwolf --new seagate                    # auto-detects single missing device
sudo braid replace --old ironwolf --new seagate --missing-id 3     # explicit devid when multiple missing
```

Use `braid status --verbose` to see device IDs. If the pool has missing devices when you try a live replace, repair the missing device first with `braid replace --missing-id <devid>`. Use `braid remove-missing` only to intentionally forget a stale entry without rebuilding data. When replacing a missing device and clearing the last missing entry with ≥2 devices remaining, a soft RAID1 balance runs automatically to restore redundancy.

### Disk identity map

Braid maintains an advisory disk identity map at `/var/lib/braid/disk-map.json`, recording each disk's `name`, `by_id`, `luks_uuid`, and `devid`. This is updated automatically by `add`, `remove`, `replace`, and `remove-missing` commands. It is non-authoritative — live pool probing is always the source of truth — and is rebuilt by normal command executions.

In v1.0, disk names are immutable once recorded in this map. Renaming/reassigning a name in config is rejected by mutating commands. Keep the original name, or use explicit `braid replace` / `braid remove` + `braid add` workflows.

### Pool status

```sh
sudo braid status             # pool health summary
sudo braid status --verbose   # per-disk detail with devids
sudo braid status --json      # machine-readable output
```

Drive states:

- `present` — disk is in the pool and online
- `new` — declared in config but not yet added (`braid add`)
- `missing` — was in pool but device is absent (unplugged, failed, powered off)
- `unknown` — cannot determine (disk-map unreadable)

Drive state classification uses the disk identity map. If the map is unavailable, absent disks show as `unknown` instead of `new` or `missing`.

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

### Non-interactive mode

For scripting, use `--passphrase-stdin` or `--passphrase-file` with `--yes`:

```sh
echo 'secret' | sudo braid add ironwolf --passphrase-stdin --yes
sudo braid add ironwolf --yes --passphrase-file /run/secrets/luks
```

## Pool unlock

After boot, bring the encrypted pool online:

```sh
systemctl start braid-pool.target
```

One passphrase prompt opens all available LUKS devices and mounts the pool. Works from TTY, SSH, or scripted. If disks are missing, use `--allow-degraded` to mount with reduced redundancy.

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
sudo braid add ironwolf --enroll /mnt/usb
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
braid <TAB>           # → add  remove  remove-missing  replace  status  doctor
braid add <TAB>       # → toshiba  ironwolf  seagate
braid add --<TAB>     # → --dry-run  --yes  --passphrase-file  --progress
```

Disk name candidates are read from `/etc/braid/config.json` on every tab press, so they reflect your current `braid.disks` config after a `nixos-rebuild`.

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
