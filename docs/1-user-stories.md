# User Stories

## Story 1: First disk, start copying data immediately

Dan buys a NAS computer and installs NixOS on its internal SSD. He has one 12TB Toshiba SATA drive. His second 12TB Ironwolf arrives next week, but he wants to start moving data off his Synology now.

### Setup

1. Plug in the 12TB Toshiba. Find its by-id path:
   ```
   $ ls /dev/disk/by-id/ata-*
   ```

2. Add braid to the NixOS config with the disk:
   ```nix
   braid = {
     enable = true;
     disks = [
       "/dev/disk/by-id/ata-Toshiba_MN07_XXXX"
     ];
   };
   ```

3. `nixos-rebuild switch` — module writes config, creates LUKS entries (which fail gracefully since the disk isn't formatted yet).

4. Format and add it to the pool:
   ```
   $ sudo braid init-disk /dev/disk/by-id/ata-Toshiba_MN07_XXXX
   $ sudo braid apply
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
   $ ls /dev/disk/by-id/ata-*
   ```

9. Add the new disk to the NixOS config:
   ```nix
   braid = {
     disks = [
       "/dev/disk/by-id/ata-Toshiba_MN07_XXXX"
       "/dev/disk/by-id/ata-Ironwolf_ST12_YYYY"
     ];
     # ... same samba/unlock config
   };
   ```

10. `nixos-rebuild switch` — module now knows about both disks for unlock at boot.

11. Preview and apply:
    ```
    $ sudo braid plan
    Plan ID: 2024-01-15T10:30:00Z-a1b2c3
    Mount:   /mnt/storage

    Actions:
      ADD_DISK_LUKS_FORMAT_OPEN  /dev/disk/by-id/ata-Ironwolf_ST12_YYYY
      ADD_DISK_BTRFS_ADD         /dev/disk/by-id/ata-Ironwolf_ST12_YYYY
      BALANCE_TO_RAID1           /mnt/storage

    $ sudo braid apply
    ```

12. Samba share never went down. Files still at `smb://nas/storage`. The Mac never noticed anything changed.

### Three months later: third drive on sale

13. Same flow — plug in, add to config, rebuild, `braid plan`, `braid apply`. Pool grows to ~18TB usable with RAID1 across 3 drives.

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
