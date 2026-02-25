# braid

NixOS module for encrypted NAS storage with auto-healing and dynamic drive pooling.

- **LUKS** full disk encryption with SSH remote unlock
- **btrfs RAID1** with checksumming and automatic self-healing
- Dynamic pool — add or remove drives without reformatting

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

### Start with one disk

```nix
braid = {
  enable = true;
  disks.toshiba = { byId = "/dev/disk/by-id/ata-Toshiba_MN07_XXXX"; };
};
```

```
sudo nixos-rebuild switch
sudo braid add toshiba
```

The pool is live immediately. No redundancy yet — data is available but unprotected until a second drive is added.

### Add a drive

```nix
braid.disks.ironwolf = { byId = "/dev/disk/by-id/ata-Ironwolf_ST12_YYYY"; };
```

```
sudo nixos-rebuild switch
sudo braid add ironwolf
```

The pool converts to RAID1 automatically. Existing data rebalances in the background.

### Preview before executing

```
$ sudo braid add ironwolf --dry-run
[destructive] LUKS format /dev/disk/by-id/ata-Ironwolf_ST12_YYYY
[safe       ] LUKS open → braid-ironwolf
[safe       ] btrfs device add /dev/mapper/braid-ironwolf /mnt/storage
[long       ] btrfs balance to RAID1
```

Steps reflect actual disk state — if the disk is already LUKS-formatted, the destructive step is omitted.

### Remove a drive

```
sudo braid remove ironwolf
```

Data migrates off the drive before it's detached. If removing would leave a single disk (losing redundancy), confirmation is required. After removing, update config and rebuild:

```nix
# Remove from braid.disks, then:
sudo nixos-rebuild switch
```

### Remove a missing/dead device

```
sudo braid remove-missing                    # 1 missing device: auto-detected
sudo braid remove-missing --missing-id 3     # multiple missing: target by devid
```

Use `braid status --verbose` to see device IDs.

### Replace a failed drive

```nix
# Add replacement to config:
braid.disks.seagate = { byId = "/dev/disk/by-id/ata-Seagate_NEW_ZZZZ"; };
# (keep dead disk in config until replace completes, then remove it)
```

```
sudo nixos-rebuild switch
sudo braid replace --old ironwolf --new seagate
```

The new drive is added and rebalanced **before** the dead device is evicted. Redundancy never drops.

### Disk identity map

Braid maintains an advisory disk identity map at `/var/lib/braid/disk-map.json`, recording each disk's `name`, `by_id`, `luks_uuid`, and `devid`. This is updated automatically by `add`, `remove`, `replace`, and `remove-missing` commands. It is non-authoritative — live pool probing is always the source of truth — and is rebuilt by normal command executions.

In v1.0, disk keys are immutable once recorded in this map. Renaming/reassigning a key in config is rejected by mutating commands. Keep the original key, or use explicit `braid replace` / `braid remove` + `braid add` workflows.

### Pool status

```
sudo braid status             # pool health summary
sudo braid status --verbose   # per-disk detail with devids
sudo braid status --json      # machine-readable output
```

### Diagnostics

```
sudo braid doctor           # check config, pool health, etc.
sudo braid doctor --json    # machine-readable output
```

### Non-interactive mode

For scripting, use `--yes` with either `BRAID_PASSPHRASE` env var or `--passphrase-file`:

```
sudo BRAID_PASSPHRASE='secret' braid add ironwolf --yes
sudo braid add ironwolf --yes --passphrase-file /run/secrets/luks
```

### Resume an interrupted operation

If a long-running operation (balance, device remove) is interrupted, re-run the same command. The checkpoint resumes where it left off:

```
sudo braid add ironwolf     # interrupted during balance
sudo braid add ironwolf     # resumes balance from where it stopped
```

Resume validation is strict and fail-closed:

```
error[CHECKPOINT_CONFIG_DRIFT]: config changed since checkpoint was created
```

Invalid checkpoints never auto-continue. Update config/pool to match, or complete the original operation intent first.

## Post-boot pool unlock

If you missed the initrd SSH unlock window, bring the pool online from a normal session:

```
systemctl start braid-pool.target
```

One passphrase prompt opens all available LUKS devices and mounts the pool. Works from TTY, SSH, or scripted. Tolerates missing/dead disks (mounts degraded).

## Shell Completions

Tab completion for subcommands, flags, and disk names works out of the box on NixOS when `braid.enable = true`. Completions are registered for bash, zsh, and fish.

```
braid <TAB>           # → add  remove  remove-missing  replace  status  doctor
braid add <TAB>       # → toshiba  ironwolf  seagate
braid add --<TAB>     # → --dry-run  --yes  --passphrase-file  --progress
```

Disk name candidates are read from `/etc/braid/config.json` on every tab press, so they reflect your current `braid.disks` config after a `nixos-rebuild`.

## Remote Unlock (SSH)

Unlock LUKS disks over SSH during early boot so you don't need physical access.

### Setup

1. Generate an initrd SSH host key:

```
sudo mkdir -p /etc/secrets/initrd
sudo ssh-keygen -t ed25519 -f /etc/secrets/initrd/ssh_host_ed25519_key -N ''
```

2. Enable remote unlock in your config:

```nix
braid = {
  enable = true;
  disks = { ... };
  remoteUnlock = {
    enable = true;
    authorizedKeys = [
      "ssh-ed25519 AAAA... you@host"  # ~/.ssh/id_ed25519.pub from your client machine
    ];
    # sshPort = 2222;    # default
    # hostKeys = [ "/etc/secrets/initrd/ssh_host_ed25519_key" ];  # default
  };
};
```

3. Rebuild: `sudo nixos-rebuild switch`

### Unlocking

From your client machine:

```
ssh -p 2222 root@<hostname>
```

You'll be dropped into the initrd shell where you can enter the LUKS passphrase. Boot continues after unlock.

## What you get for free

Braid enables these automatically when `braid.enable = true`:

- **Monthly btrfs scrub** — detects and repairs bit rot before it can compound. Override or disable with normal NixOS config:

  ```nix
  # change to weekly
  services.btrfs.autoScrub.interval = "weekly";

  # or disable
  services.btrfs.autoScrub.enable = false;
  ```

- **Resilient boot** — a dead or missing drive never blocks boot. The pool mounts in degraded mode and the system stays reachable via SSH.

- **Pinned toolchain** — runtime tools (btrfs-progs, cryptsetup, util-linux) are pinned to a NixOS stable release. Parser output formats don't change on flake updates. Override individual tools via `braid.packages.*` if needed.

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
just test first-boot-single-disk

# run a few specific tests
just test first-boot-single-disk braid-remove-disk

# add verbose VM logs
just test first-boot-single-disk -v

# run full suite before finishing
just test
```

Rust CLI code lives in `cli/`. Build it directly with:

```bash
nix build .#braid
```
