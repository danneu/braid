# Test: braid-discover-migration
#
# Intent: exercise the legacy name-keyed pool.json cutover runbook end to end.
#
# Why it exists: operators need to preview legacy state safely, refuse blind
# overwrites, fail closed on both missing and extra discovered members, and
# finish with a UUID-keyed pool.json that unlocks the pool.
#
# Scenario: a pre-LUKS-UUID-identity NAS boots with three braid-labeled LUKS
# disks and an old name-keyed pool.json, then the operator migrates by moving
# the old file aside and rerunning discover with --expect-count.
{ braid }:
{ pkgs, ... }:
let
  passphrase = "testpassphrase";
  diskNames = [
    "disk1"
    "disk2"
    "disk3"
  ];
in
{
  name = "braid-discover-migration";

  nodes.machine =
    { pkgs, ... }:
    {
      imports = [
        ../../modules/braid
        (import ../module/lib/initrd-fixture.nix {
          inherit passphrase diskNames;
          description = "Prepare 3-disk LUKS + btrfs RAID1 fixture for discover migration test";
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
        {
          size = 256;
          driveConfig.deviceExtraOpts.serial = "disk3";
        }
      ];
      virtualisation.memorySize = 2048;

      environment.systemPackages = [ pkgs.btrfs-progs ];
    };

  testScript = builtins.readFile ./braid-discover-migration.py;
}
