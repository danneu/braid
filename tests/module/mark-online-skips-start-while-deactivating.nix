# Test: mark-online-skips-start-while-deactivating
#
# What: mark_online skips systemctl start when the snapshot saw
# braid-online.service deactivating.
#
# Why: Starting behind an in-flight stop can deadlock the lifecycle unit.
{ braid }:
let
  passphrase = "testpassphrase";
in
{
  name = "mark-online-skips-start-while-deactivating";

  nodes.machine =
    { ... }:
    {
      imports = [
        ../../modules/braid
        (import ./lib/initrd-fixture.nix {
          inherit passphrase;
          diskNames = [ "disk1" ];
          description = "Prepare one-disk LUKS + btrfs fixture for deactivating snapshot test";
        })
      ];

      braid = {
        enable = true;
        package = braid;
      };

      systemd.tmpfiles.rules = [
        "d /var/lib/braid 0755 root root -"
        ''f /var/lib/braid/pool.json 0644 root root - {"disks":{"11111111-1111-1111-1111-111111111111":{"name":"disk1","by_id":"/dev/disk/by-id/virtio-disk1"}}}''
      ];

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

  testScript = builtins.readFile ./mark-online-skips-start-while-deactivating.py;
}
