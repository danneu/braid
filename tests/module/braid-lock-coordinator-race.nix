# Test: braid-lock-coordinator-race
#
# What: external systemctl stop racing plain `braid lock` completes without
# coordinator deadlock.
#
# Why: The stop coordinator's done-marker protocol is what makes synchronous
# post-lock unit stop safe.
{ braid }:
let
  passphrase = "testpassphrase";
  diskNames = [
    "disk1"
    "disk2"
  ];
in
{
  name = "braid-lock-coordinator-race";

  nodes.machine =
    { pkgs, ... }:
    {
      imports = [
        ../../modules/braid
        (import ./lib/initrd-fixture.nix {
          inherit passphrase diskNames;
          description = "Prepare LUKS + btrfs fixture for coordinator race test";
        })
      ];

      braid = {
        enable = true;
        package = braid;
      };

      systemd.tmpfiles.rules = [
        "d /var/lib/braid 0755 root root -"
        ''f /var/lib/braid/pool.json 0644 root root - {"disks":{"11111111-1111-1111-1111-111111111111":{"name":"disk1","by_id":"/dev/disk/by-id/virtio-disk1"},"22222222-2222-2222-2222-222222222222":{"name":"disk2","by_id":"/dev/disk/by-id/virtio-disk2"}}}''
      ];

      systemd.services.dummy-slow-consumer = {
        description = "Fake bound consumer with slow stop for coordinator race";
        wantedBy = [ "braid-online.service" ];
        after = [ "braid-online.service" ];
        bindsTo = [ "braid-online.service" ];
        unitConfig.ConditionPathIsMountPoint = "/mnt/storage";
        serviceConfig = {
          Type = "simple";
          ExecStart = "${pkgs.coreutils}/bin/sleep 300";
          ExecStop = "${pkgs.coreutils}/bin/sleep 5";
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
      virtualisation.memorySize = 1024;
    };

  testScript = builtins.readFile ./braid-lock-coordinator-race.py;
}
