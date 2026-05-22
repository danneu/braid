[← Manual](../index.md)

# Mounting subvolumes

Mount a btrfs subvolume at a custom path when a person or service should see
one part of the pool as its own filesystem. Common examples are a friendlier
path under `/home`, or a media path like `/var/lib/jellyfin/media`.

## How braid mounts the pool

braid mounts the btrfs top-level subvolume (`subvolid=5`) at
`/mnt/storage` by default. Treat that as the management mount: it is where you
create subvolumes, run btrfs commands, and manage the whole pool. Consumer
services do not need access to that mount root.

## The `subvol=` mount idiom

btrfs can mount a subvolume directly with `subvol=<path>`. The btrfs docs
describe the important isolation property this way: "the parent directory is
not visible and accessible", which is "similar to a bind mount".

For braid, that means a service can see only `movies` at
`/var/lib/jellyfin/media` without needing permission to traverse
`/mnt/storage`.

## Recipe: mount a subvolume at a custom path

Create the subvolume while the pool is unlocked:

```sh
sudo btrfs subvolume create /mnt/storage/movies
```

Find the btrfs filesystem UUID:

```sh
sudo btrfs filesystem show /mnt/storage
```

Use the `uuid:` line from that output. `braid status` does not currently show
the btrfs filesystem UUID.

Add a native systemd mount unit to your NixOS configuration:

```nix
systemd.mounts = [{
  what = "/dev/disk/by-uuid/<btrfs-fs-uuid>";
  where = "/home/dan/my-movies";
  type = "btrfs";
  options = "subvol=movies,ro,noatime";
  wantedBy = [ "braid-online.service" ];
  bindsTo = [ "braid-online.service" ];
  after = [ "braid-online.service" ];
}];
```

Field notes:

- `what` points at the btrfs filesystem UUID, not an individual LUKS disk.
- `where` is the path where the subvolume should appear.
- `type = "btrfs"` selects the btrfs mount helper.
- `options` selects the `movies` subvolume. `ro` is optional but recommended
  for read-only consumers.
- `wantedBy` starts the mount when `braid-online.service` activates after
  `braid unlock`.
- `bindsTo` is the load-bearing lifecycle edge. It puts the mount unit in
  `BoundBy braid-online.service`, which is what `braid lock` stops before
  unmounting the pool.
- `after` orders mount startup after `braid-online.service`, so the btrfs
  `/dev/disk/by-uuid` symlink exists before systemd resolves `what`.

Rebuild and verify:

```sh
sudo nixos-rebuild switch
findmnt /home/dan/my-movies
systemctl show -P BoundBy braid-online.service
```

The escaped mount unit name, for example `home-dan-my\x2dmovies.mount`, should
appear in `BoundBy braid-online.service`.

## `subvol=` vs bind mount

Both approaches can expose a subtree at another path. `subvol=` is the better
default for braid because it is conventional btrfs configuration, it mounts the
subvolume directly, and it does not require the consumer to traverse
`/mnt/storage`.

Use a bind mount only when the consumer already has permission to traverse the
source mount and you need the same mounted data at multiple paths.

## Why not `fileSystems` with `x-systemd.requires`?

`fileSystems` is fstab-shaped. systemd's fstab options can express
`Requires=` and `After=`, but not an arbitrary `BindsTo=braid-online.service`
edge. Without `BindsTo`, the mount is not listed in
`BoundBy braid-online.service`, so `braid lock` will not stop it before
unmounting the pool. Use native `systemd.mounts` for lifecycle-bound subvolume
mounts. See [ADR 018](../../docs/decisions/018-systemd-lifecycle.md) for the
lifecycle model.

## Worked example: read-only access for Jellyfin

Create the media subvolume:

```sh
sudo btrfs subvolume create /mnt/storage/movies
```

Mount it where Jellyfin expects media:

```nix
systemd.mounts = [{
  what = "/dev/disk/by-uuid/<btrfs-fs-uuid>";
  where = "/var/lib/jellyfin/media";
  type = "btrfs";
  options = "subvol=movies,ro,noatime";
  wantedBy = [ "braid-online.service" ];
  bindsTo = [ "braid-online.service" ];
  after = [ "braid-online.service" ];
}];
```

Grant Jellyfin read-only traversal on the subvolume contents:

```sh
sudo setfacl -R    -m u:jellyfin:rx /mnt/storage/movies
sudo setfacl -R -d -m u:jellyfin:rx /mnt/storage/movies
```

Do not add `jellyfin` to `storage`. That would grant the daemon read-write
access across the whole pool. The ACL above scopes read access to one
subvolume, and the `subvol=` mount means Jellyfin does not need to traverse
`/mnt/storage` itself.

Bind Jellyfin to the mount unit:

```nix
services.jellyfin = {
  enable = true;
  openFirewall = true;
};

systemd.services.jellyfin = {
  wantedBy = lib.mkForce [ "var-lib-jellyfin-media.mount" ];
  bindsTo = [ "var-lib-jellyfin-media.mount" ];
  after = [ "var-lib-jellyfin-media.mount" ];
  unitConfig.ConditionPathIsMountPoint = "/var/lib/jellyfin/media";
};
```

Bind the service to `var-lib-jellyfin-media.mount`, not directly to
`braid-online.service`. That ensures Jellyfin starts only after its media path
is mounted. During `braid lock`, systemd stops Jellyfin first, then the
subvolume mount, then braid unmounts the management mount and closes LUKS.

The full triad pattern is the same lifecycle shape described in
[Sharing and permissions](sharing-and-permissions.md#binding-shares-to-the-pool-lifecycle).

Verify:

```sh
sudo -u jellyfin ls /var/lib/jellyfin/media
sudo braid lock
systemctl is-active jellyfin.service
systemctl is-active var-lib-jellyfin-media.mount
```

Point the Jellyfin web UI at `/var/lib/jellyfin/media`. After `braid lock`,
both units should be inactive and the LUKS devices should be closed.

## Future enhancement

braid already tracks the btrfs FSID internally (`PoolState.fsid` in
`cli/src/types.rs`), but `braid status` does not render it yet. Showing it in
human and JSON status output would remove the `btrfs filesystem show` lookup.

## What's next

- [Sharing and permissions](sharing-and-permissions.md)
- [Day-to-day NAS usage](day-to-day-nas-usage.md)

## Related commands

- [unlock](../commands/unlock.md)
- [status](../commands/status.md)
