We want a daemon that is always runnng that we can query. Then we can build a
TUI on top of it (queries the daemon) and we can have helper scripts that
actually jsut make calls to the daemon.

It will run as systemd service and expose newline-delimited JSON over unix
socket.

The live dashboard TUI will be a separate binary that connects to that socket.

Remember: nix config is the source of truth. We want to do nixos-idiomatic
approaches.

## Phase 1: Read only queries

At first, our daemon will do read-only stuff like query btrfs, cryptsetup,
lsblk, systemctl. And serve the disk pool status over unix socket.

TUI connects to socket for live dashboard.

## Phase 2: Mutable operations that nix config cannot do

For example, if the user wants to format usb drives as luks keyfiles, that's a
mutable op that they can't do via nix config.

## Phase 3: Mutable operations that must be added to nix config

We'd want the user to add/remove drives through a TUI. But they'd also be
responsible for updating their nix config to add/remove the drive.

## Things we can query the daemon for:

- Describe pool health: ok | degraded | unavailable?
- List drives in pool
- Per drive:
  - health
  - list luks keys (assume slot 0 is passphrase, slot 1+ are keyfiles)

### Give info about usb keyfiles

You could scan for USB block devices and check each one:

1. `ls /dev/disk/by-id/usb-\*` — lists all USB storage devices
2. Mount each one, look for a known filename like btrnas.key
3. `cryptsetup luksOpen --test-passphrase --key-file=btrnas.key /dev/disk/by-id/ata-Toshiba_MN07_xxxx` — tests if the key actually works without opening the device

So a btrnas-status command could show:

```
  Pool: /mnt/storage (mounted, healthy)
  Drives:
    ata-Toshiba_MN07_xxxx  — 2 key slots active (0, 1)
    ata-Ironwolf_ST12_xxxx — 2 key slots active (0, 1)
  USB keys:
    usb-SanDisk_Ultra_xxxx — valid key found ✓
    usb-Kingston_DT50_xxxx — no btrnas.key
```

The `--test-passphrase` flag is the key piece — it validates without actually unlocking anything.
