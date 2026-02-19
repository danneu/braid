# btrnas

NixOS module for encrypted NAS storage with auto-healing and dynamic drive pooling.

- **LUKS** full disk encryption with SSH remote unlock
- **btrfs RAID1** with checksumming and automatic self-healing
- Dynamic pool — add or remove drives without reformatting

## Example

```nix
btrnas = {
  enable = true;
  disks = [
    "/dev/disk/by-id/ata-Toshiba_MN07_XXXX"
    "/dev/disk/by-id/ata-Ironwolf_ST12_YYYY"
  ];
  mountPoint = "/mnt/storage";  # default
};
```

This creates LUKS devices for each disk, assembles them into a btrfs RAID1 pool, and mounts it. The system boots gracefully even if a drive is dead or missing.

## Samba

Samba is not part of the btrnas module — NixOS already provides declarative Samba config. Reference `config.btrnas.mountPoint` to stay in sync:

```nix
services.samba = {
  enable = true;
  openFirewall = true;
  settings = {
    videos = {
      path = "${config.btrnas.mountPoint}/videos";
      "guest ok" = "yes";
      "read only" = "yes";
      browseable = "yes";
    };
    poetry = {
      path = "${config.btrnas.mountPoint}/poetry";
      "guest ok" = "no";
      "valid users" = "dan";
      browseable = "no";
    };
  };
};
```

Change `btrnas.mountPoint` and Samba follows automatically — no extra module code needed, just a Nix expression referencing an existing option.

After rebuilding, create the Samba password (one-time):

```
sudo smbpasswd -a dan
```

Then from macOS: Finder → Cmd+K → `smb://nas/videos`.
