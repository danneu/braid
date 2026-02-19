# User Stories

## Story 1: First disk, start copying data immediately

Dan buys a NAS computer and installs NixOS on its internal SSD. He has one 12TB Toshiba SATA drive. His second 12TB Ironwolf arrives next week, but he wants to start moving data off his Synology now.

### Setup

1. Plug in the 12TB Toshiba.

2. Format and add it to the pool:
   ```
   $ sudo btrnas-add-disk /dev/disk/by-id/ata-Toshiba_MN07_XXXX

   WARNING: This will PERMANENTLY ERASE all data on:
     /dev/disk/by-id/ata-Toshiba_MN07_XXXX (Toshiba MN07ACA12T, 12TB)

   It will be LUKS-encrypted and added to the btrfs pool at /mnt/storage.

   Type 'erase this disk' to confirm: erase this disk

   Formatting LUKS...
   Creating btrfs filesystem...
   Mounting at /mnt/storage...
   Done. No redundancy yet — add a second disk for RAID1 protection.
   ```

3. Add btrnas to the NixOS config:
   ```nix
   btrnas = {
     disks = [
       "/dev/disk/by-id/ata-Toshiba_MN07_XXXX"
     ];
     samba = {
       enable = true;
       user = "dan";
       passwordFile = "/run/secrets/smb-password";
     };
     remoteUnlock = {
       enable = true;
       sshPort = 2222;
       authorizedKeys = [ "ssh-ed25519 AAAA..." ];
     };
   };
   ```

4. `nixos-rebuild switch` — btrnas sees the already-formatted LUKS disk, configures unlock + mount + Samba. No formatting happens here, ever.

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

9. Add it to the pool:
   ```
   $ sudo btrnas-add-disk /dev/disk/by-id/ata-Ironwolf_ST12_YYYY

   WARNING: This will PERMANENTLY ERASE all data on:
     /dev/disk/by-id/ata-Ironwolf_ST12_YYYY (Seagate IronWolf ST12000VN0008, 12TB)

   It will be LUKS-encrypted and added to the btrfs pool at /mnt/storage.
   Existing data will be rebalanced to RAID1 (this may take hours).

   Type 'erase this disk' to confirm: erase this disk

   Formatting LUKS...
   Adding to btrfs pool...
   Converting to RAID1 (background)...
   Done. Pool now has 2 disks with RAID1 redundancy.
   ```

10. Add the new disk to the NixOS config:
    ```nix
    btrnas = {
      disks = [
        "/dev/disk/by-id/ata-Toshiba_MN07_XXXX"
        "/dev/disk/by-id/ata-Ironwolf_ST12_YYYY"
      ];
      # ... same samba/unlock config
    };
    ```

11. `nixos-rebuild switch` — btrnas now knows about both disks for unlock at boot.

12. Samba share never went down. Files still at `smb://nas/storage`. The Mac never noticed anything changed.

### Three months later: third drive on sale

13. Same flow — plug in, `btrnas-add-disk`, add to config, rebuild. Pool grows to ~18TB usable with RAID1 across 3 drives.

## Design principles shown here

- **`nixos-rebuild switch` never formats or destroys data.** It is declarative and idempotent — "this is what my system looks like." Safe to run anytime.
- **`btrnas-add-disk` is the only destructive action.** It is imperative and intentional, with a scary confirmation prompt. You run it once per new disk.
- **The module handles:** LUKS unlock at boot, btrfs mount, Samba serving, remote SSH unlock.
- **The user handles:** physical disk installation, running `btrnas-add-disk`, adding the disk path to their NixOS config.
