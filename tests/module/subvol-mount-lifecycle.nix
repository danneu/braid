/*
  Intent: A documented `systemd.mounts` subvolume mount participates in the
    `braid lock` BoundBy cascade and starts again on the next unlock.
  Why it exists: The mounting-subvolumes guide tells users to bind native
    mount units to braid-online.service. This guards the lifecycle contract
    against regressions in systemd wiring or lock teardown behavior.
  Scenario: The guide in docs/guides/mounting-subvolumes.md documents a
    read-only Jellyfin-style subvolume mount with a service bound to the mount.
    The test unlocks, proves both units start, locks while the service holds the
    subvolume mount busy, and then unlocks again to prove reactivation works.
*/
{ braid }:
{ pkgs, lib, ... }:
let
  passphrase = "testpassphrase";
  diskNames = [
    "disk1"
    "disk2"
  ];
in
{
  name = "subvol-mount-lifecycle";

  nodes.machine =
    { pkgs, lib, ... }:
    {
      imports = [
        ../../modules/braid
        (import ./lib/initrd-fixture.nix {
          inherit passphrase diskNames;
          supportedFilesystems = [ "btrfs" ];
          btrfsFsid = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
          description = "Prepare LUKS + btrfs fixture with movies subvolume for mount-lifecycle test";
          preCloseScript = ''
            mkdir -p /tmp/fixture-mount
            mount /dev/mapper/braid-disk1-fmt /tmp/fixture-mount
            btrfs subvolume create /tmp/fixture-mount/movies
            # Pre-create the busy-mount probe file. Mode 644 lets the
            # unprivileged dummy-jellyfin service open it read-only through
            # the ro subvolume mount.
            touch /tmp/fixture-mount/movies/.consumer-lock
            chmod 0644 /tmp/fixture-mount/movies/.consumer-lock
            sync
            umount /tmp/fixture-mount
          '';
        })
      ];

      braid = {
        enable = true;
        package = braid;
      };

      systemd.tmpfiles.rules = [
        "d /var/lib/braid 0755 root root -"
        ''f /var/lib/braid/pool.json 0644 root root - {"disks":{"11111111-1111-1111-1111-111111111111":{"name":"disk1","by_id":"/dev/disk/by-id/virtio-disk1"},"22222222-2222-2222-2222-222222222222":{"name":"disk2","by_id":"/dev/disk/by-id/virtio-disk2"}}}''
        "d /var/lib/jellyfin 0755 root root -"
        "d /var/lib/jellyfin/media 0755 root root -"
      ];

      systemd.services.braid-unlock.script = lib.mkForce ''
        printf '%s\n' '${passphrase}' | braid unlock --passphrase-stdin
      '';

      systemd.mounts = [
        {
          what = "/dev/disk/by-uuid/aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
          where = "/var/lib/jellyfin/media";
          type = "btrfs";
          options = "subvol=movies,ro,noatime";
          wantedBy = [ "braid-online.service" ];
          bindsTo = [ "braid-online.service" ];
          after = [ "braid-online.service" ];
        }
      ];

      users.groups.dummy-jellyfin = { };
      users.users.dummy-jellyfin = {
        isSystemUser = true;
        group = "dummy-jellyfin";
      };

      systemd.services.dummy-jellyfin = {
        description = "Fake Jellyfin service that holds the subvolume mount busy";
        wantedBy = [ "var-lib-jellyfin-media.mount" ];
        bindsTo = [ "var-lib-jellyfin-media.mount" ];
        after = [ "var-lib-jellyfin-media.mount" ];
        unitConfig.ConditionPathIsMountPoint = "/var/lib/jellyfin/media";
        serviceConfig = {
          Type = "simple";
          User = "dummy-jellyfin";
          ExecStart = pkgs.writeShellScript "dummy-jellyfin" ''
            exec 3</var/lib/jellyfin/media/.consumer-lock
            sleep 300
          '';
        };
      };

      virtualisation.emptyDiskImages = [
        {
          size = 512;
          driveConfig.deviceExtraOpts.serial = "disk1";
        }
        {
          size = 512;
          driveConfig.deviceExtraOpts.serial = "disk2";
        }
      ];
      virtualisation.memorySize = 2048;

      environment.systemPackages = [
        pkgs.btrfs-progs
        pkgs.cryptsetup
      ];
    };

  testScript = builtins.readFile ./subvol-mount-lifecycle.py;
}
