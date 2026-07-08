[← braid](../index.md)

# Sharing and permissions

How braid manages mount point permissions and how to share the pool over the network. Read this when setting up file access for multiple users or configuring Samba/NFS.

## Mount point permissions

After every mount-producing command (`unlock`, `add`, `recover`), braid sets the mount root to:

```
root:storage 2770
```

This means:

- **Owner (root):** full access
- **Group (storage):** read, write, execute
- **Others:** no access
- **Setgid bit (2):** new files and directories inherit the `storage` group

Any user in the `storage` group can read and write files under the mount point. The setgid bit ensures that files created by any group member are owned by the `storage` group, so all members can manage each other's files.

## Adding users to the storage group

```nix
# configuration.nix
users.users.alice = {
  isNormalUser = true;
  extraGroups = [ config.braid.poolAccessGroup ];
};

users.users.bob = {
  isNormalUser = true;
  extraGroups = [ config.braid.poolAccessGroup ];
};
```

Using `config.braid.poolAccessGroup` instead of the literal `"storage"` keeps the reference correct if you customize the group name.

For network-facing services like Jellyfin or Plex that should only read a
single subtree, prefer mounting that subvolume separately and using POSIX ACLs
over adding the service to the storage group. See
[Mounting subvolumes](mounting-subvolumes.md) for the recipe.

## Custom group name

```nix
braid.poolAccessGroup = "nas";
```

The group is created automatically. All behavior (mount permissions, setgid) works the same with any valid Unix group name.

## Disabling the storage group

```nix
braid.poolAccessGroup = null;
```

When `null`, braid does not create a group or set permissions on the mount point after unlock. You manage permissions yourself.

## Umask note

The setgid bit on the mount root ensures new files get the correct group. But the creating user's umask controls the permission bits. The default umask (`022`) produces files with mode `644` (owner write, group/other read-only).

For collaborative write access where all group members can edit each other's files, set a more permissive umask for processes that write to the pool:

```nix
# In a Samba share definition (see below), force create mode handles this.
# For SSH users, set umask in their shell profile:
programs.bash.interactiveShellInit = ''
  umask 002
'';
```

With umask `002`, new files are `664` (owner and group read-write, other read-only) and new directories are `775`.

## Samba integration

Samba is not part of the braid module, but it works well with the braid mount point. Here is a declarative NixOS Samba config:

```nix
# configuration.nix
services.samba = {
  enable = true;
  openFirewall = true;

  settings = {
    global = {
      workgroup = "WORKGROUP";
      "server string" = "NAS";
      security = "user";
    };

    storage = {
      path = config.braid.mountPoint;
      browseable = "yes";
      "read only" = "no";
      "valid users" = "@${config.braid.poolAccessGroup}";

      # File permissions for Samba-created files
      "create mask" = "0664";
      "force create mode" = "0664";
      "directory mask" = "2775";
      "force directory mode" = "2775";
    };
  };
};

# Set Samba passwords (run once per user):
#   sudo smbpasswd -a alice
```

Key points:

- `valid users = @storage` restricts the share to the storage group.
- `force create mode` and `force directory mode` ensure group-writable permissions regardless of the client's umask.
- New files and directories inherit the `storage` group from the setgid bit
  braid sets on the mount root -- a kernel behavior that does not require
  `inherit permissions`. `force directory mode = 2775` keeps that setgid bit on
  Samba-created subdirectories so inheritance carries down the tree.
- Samba users must also be system users in the storage group.

### Multiple shares

Create separate shares for different directories under the mount point:

```nix
services.samba.settings = {
  photos = {
    path = "${config.braid.mountPoint}/photos";
    browseable = "yes";
    "read only" = "no";
    "valid users" = "@${config.braid.poolAccessGroup}";
    "create mask" = "0664";
    "force create mode" = "0664";
    "directory mask" = "2775";
    "force directory mode" = "2775";
  };

  media = {
    path = "${config.braid.mountPoint}/media";
    browseable = "yes";
    "read only" = "yes";  # read-only share
    "valid users" = "@${config.braid.poolAccessGroup}";
  };
};
```

### Binding shares to the pool lifecycle

By default, `samba-smbd.service` (the systemd unit NixOS creates from `services.samba.enable`) keeps running after `braid lock`. If a client is mid-transfer when you lock, `umount` blocks until the file handle is released. Wire the share into the pool lifecycle so systemd starts `samba-smbd` after `braid unlock` and stops it again before `braid lock` runs `umount`:

```nix
braid.poolBoundServices = [ "samba-smbd" ];
```

That option appends this contract to `systemd.services.samba-smbd`:

```nix
systemd.services.samba-smbd = {
  # Start smbd when braid marks the pool online after a successful unlock.
  wantedBy = [ "braid-online.service" ];
  # Stop smbd when braid-online stops, before braid lock unmounts the pool.
  bindsTo = [ "braid-online.service" ];
  # Order smbd on the correct side of braid-online start and stop jobs.
  after = [ "braid-online.service" ];
  # Skip boot or direct starts when the braid mount point is not mounted.
  unitConfig.ConditionPathIsMountPoint = config.braid.mountPoint;
};
```

All four fields are load-bearing and do different jobs:

- `wantedBy` -- `samba-smbd` starts when `braid-online.service` starts (i.e. after `braid unlock`).
- `bindsTo` -- `samba-smbd` stops if `braid-online.service` stops or goes inactive (i.e. before `braid lock` runs `umount`).
- `after` -- ordering only, ensures `samba-smbd` is started/stopped on the correct side of `braid-online.service`.
- `ConditionPathIsMountPoint` -- skips activation when the braid mount point is only an offline directory, so any start the triad did not initiate cannot serve an unmounted pool.

`braid lock` walks `systemctl show -P BoundBy braid-online.service` (the reverse of `BindsTo=`) and stops every consumer this way before unmount, and `ConditionPathIsMountPoint` keeps them from restarting against an offline pool. This is the same pattern braid's own scrub timer uses (see `modules/braid/storage.nix`).

The condition matters even with `wantedBy`: NixOS also starts Samba at boot through `samba.target` (which `samba-smbd.service` is `wantedBy`), and that boot edge would start `smbd` before any unlock. `ConditionPathIsMountPoint` is what stops it from serving the empty, offline mount directory. Only `smbd` serves files from the pool and can hold it busy during lock, so leave `samba.target`, `nmbd`, and `winbindd` untouched.

Use the bare `systemd.services.<name>` key, not the unit filename: `samba-smbd`, not `samba-smbd.service`. The module rejects suffixed names and names no other module defines, so typos fail before deployment instead of creating empty service skeletons.

## NFS

The same approach works for NFS. Export the braid mount point and control access at the network level:

```nix
services.nfs.server = {
  enable = true;
  exports = ''
    ${config.braid.mountPoint} 192.168.1.0/24(rw,sync,no_subtree_check,root_squash)
  '';
};
```

Use `no_root_squash` only for fully trusted clients; it maps remote client-root
to server-root over pool files, which `nosuid,nodev` do not prevent.

Adjust the subnet and options for your network. See `exports(5)` for the full option reference.

Add `nfs-server` to `braid.poolBoundServices` if you want NFS to stop before `braid lock` runs `umount` and start again after `braid unlock`:

```nix
braid.poolBoundServices = [ "nfs-server" ];
```

As with Samba, the generated condition gates NixOS's default `nfs-server.service` boot-start edge (`wantedBy = [ "multi-user.target" ]`) against an offline braid mount point.

## Auto-suspend integration

If you enable `braid.autoSuspend`, active SMB and NFS connections automatically block suspend. This is auto-detected from whether `services.samba` or `services.nfs.server` is enabled in your NixOS config -- no extra configuration needed.

## Related

- [NixOS configuration](nixos-configuration.md) -- `braid.poolAccessGroup` option reference
- [Getting started](getting-started.md) -- first-time pool setup
- [Mounting subvolumes](mounting-subvolumes.md) -- read-only service access to one subvolume
- [Power management](power-management.md) -- auto-suspend with SMB/NFS awareness
