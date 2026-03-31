# Test: braid-discover
#
# What: Boots a 2-disk LUKS fixture, then exercises the discover recovery
# workflow: read-only listing, --write to rebuild pool.json, unlock proof,
# and the "pool.json already exists" guard.
#
# Why: discover is the sole recovery path when pool.json is lost; it must
# produce a membership file that unlock can actually use.
#
# Dependencies: initrd-fixture (LUKS-formats both disks with braid labels).
{ braid }:
{ pkgs, ... }:
let
  passphrase = "testpassphrase";
  diskNames = [
    "disk1"
    "disk2"
  ];
in
{
  name = "braid-discover";

  nodes.machine =
    { pkgs, ... }:
    {
      imports = [
        ../../modules/braid
        (import ../module/lib/initrd-fixture.nix {
          inherit passphrase diskNames;
          description = "Prepare LUKS + btrfs RAID1 fixture for discover test";
        })
      ];

      braid = {
        enable = true;
        package = braid;
      };

      virtualisation.emptyDiskImages = [
        {
          size = 256;
          driveConfig.deviceExtraOpts.serial = "disk1";
        }
        {
          size = 256;
          driveConfig.deviceExtraOpts.serial = "disk2";
        }
      ];
      virtualisation.memorySize = 2048;

      environment.systemPackages = [ pkgs.btrfs-progs ];
    };

  testScript = builtins.readFile ./braid-discover.py;
}
