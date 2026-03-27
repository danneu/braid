# User Stories

## Story 1: First disk, start copying data immediately

Dan buys a NAS computer and installs NixOS on its internal SSD. He has one 12TB Toshiba SATA drive. His second 12TB Ironwolf arrives next week, but he wants to start moving data off his Synology now.

### Setup

1. Plug in the 12TB Toshiba. Find its by-id path:
   ```
   $ lsblk -d -o NAME,SIZE,ID-LINK
   ```

2. Add braid to the NixOS config:
   ```nix
   braid = {
     enable = true;
   };
   ```

3. `nixos-rebuild switch` — module sets up services, toolchain, and mount point.

4. Format and add it to the pool:
   ```
   $ sudo braid add toshiba=/dev/disk/by-id/ata-Toshiba_MN07_XXXX
   ```

5. From MacBook: Finder → Cmd+K → `smb://nas/storage`. Start dragging files over.

6. On reboot, SSH in and unlock the pool:
   ```
   $ ssh user@nas
   $ sudo braid unlock
   # type LUKS passphrase — all drives open, pool mounts, Samba comes up
   ```

### One week later: second drive arrives

7. Shut down NAS, plug in the Ironwolf 12TB. Boot, unlock the pool.

8. Find the new disk:
   ```
   $ lsblk -d -o NAME,SIZE,ID-LINK
   ```

9. Add it directly — no config change needed:
   ```
   $ sudo braid add ironwolf=/dev/disk/by-id/ata-Ironwolf_ST12_YYYY
   ```

10. The pool converts to RAID1 automatically. Existing data rebalances to the new disk in the background.

11. Samba share never went down. Files still at `smb://nas/storage`. The Mac never noticed anything changed.

### Three months later: third drive on sale

12. Same flow — plug in, find by-id path, `braid add seagate=/dev/disk/by-id/ata-Seagate_...`. Pool grows to ~18TB usable with RAID1 across 3 drives.

## Story 2: USB keyfile auto-unlock

Dan is tired of SSH'ing in to type the passphrase after every reboot. He sets up a USB keyfile for unattended auto-unlock.

### Setup

1. Generate a random keyfile on a USB drive:
   ```
   $ dd if=/dev/urandom of=/mnt/usb/braid.key bs=4096 count=1 iflag=fullblock
   $ chmod 400 /mnt/usb/braid.key
   ```

2. Enroll the keyfile into all pool disks:
   ```
   $ sudo braid enroll /mnt/usb/braid.key
   ```

3. Find the USB's by-id path:
   ```
   $ ls /dev/disk/by-id/usb-*
   ```

4. Enable auto-unlock in NixOS config:
   ```nix
   braid.autoUnlock = {
     enable = true;
     keyDevice = "/dev/disk/by-id/usb-Kingston_DataTraveler_XXXX-0:0";
   };
   ```

5. `nixos-rebuild switch`.

### Reboot with USB key present

6. NAS reboots. `braid-auto-unlock` service mounts the USB, opens all LUKS volumes with the keyfile, mounts the pool, then unmounts the USB. Pool is online before anyone SSH's in.

### Reboot without USB key

7. USB is removed (or lost). NAS reboots. `braid-auto-unlock` sees no USB device, prints a skip message, exits 0. Boot completes normally. Pool stays locked. Dan SSH's in and runs `sudo braid unlock` with his passphrase. Everything works — the passphrase is an independent credential.

## Design principles

See [docs/principles.md](principles.md) for the canonical list of invariants behind this workflow.
