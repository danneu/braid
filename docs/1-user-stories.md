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
     remoteUnlock = {
       enable = true;
       sshPort = 2222;
       authorizedKeys = [ "ssh-ed25519 AAAA..." ];
     };
   };
   ```

3. `nixos-rebuild switch` — module writes config, creates LUKS entries (which fail gracefully since the disk isn't formatted yet).

4. Format and add it to the pool:
   ```
   $ sudo braid-add-disk /dev/disk/by-id/ata-Toshiba_MN07_XXXX

   WARNING: This will PERMANENTLY ERASE all data on:
     /dev/disk/by-id/ata-Toshiba_MN07_XXXX (Toshiba MN07ACA12T, 12TB)

   It will be LUKS-encrypted and become the first disk in a new btrfs pool.
   NOTE: A single disk has NO redundancy. Add a second disk for RAID1 protection.

   Type 'erase this disk' to confirm: erase this disk

   Formatting LUKS...
   Creating btrfs filesystem...
   Mounting at /mnt/storage...
   Done. This disk will auto-unlock on next reboot.
   ```

5. From MacBook: Finder → Cmd+K → `smb://nas/storage`. Start dragging files over.

6. On reboot, SSH in to unlock:
   ```
   $ ssh -p 2222 root@nas
   # type LUKS passphrase
   # NAS finishes booting, Samba comes up
   ```

### One week later: second drive arrives

7. Shut down NAS, plug in the Ironwolf 12TB. Boot, SSH-unlock.

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

11. Format and add it to the pool:
    ```
    $ sudo braid-add-disk /dev/disk/by-id/ata-Ironwolf_ST12_YYYY

    WARNING: This will PERMANENTLY ERASE all data on:
      /dev/disk/by-id/ata-Ironwolf_ST12_YYYY (Seagate IronWolf ST12000VN0008, 12TB)

    It will be LUKS-encrypted and added to the btrfs pool at /mnt/storage.

    Type 'erase this disk' to confirm: erase this disk

    Formatting LUKS...
    Adding to btrfs pool...
    Converting to RAID1 (background)...
    Done. This disk will auto-unlock on next reboot.
    ```

12. Samba share never went down. Files still at `smb://nas/storage`. The Mac never noticed anything changed.

### Three months later: third drive on sale

13. Same flow — plug in, add to config, rebuild, `braid-add-disk`. Pool grows to ~18TB usable with RAID1 across 3 drives.

## Design principles

See [docs/principles.md](principles.md) for the canonical list of invariants behind this workflow.
