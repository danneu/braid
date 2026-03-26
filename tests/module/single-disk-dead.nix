# Test: braid-module-single-disk-dead
#
# What: Enables the braid module with a single disk. An initrd fixture formats
# it as LUKS + single-disk btrfs, then bricks the LUKS header. After boot,
# `braid unlock` fails (bricked LUKS). The system boots to multi-user with
# no /mnt/storage (no RAID1 fallback).
#
# Why: Validates the "all drives dead" tier on the simplest config. With only
# one drive and no RAID1, a dead drive means total data loss — but the system
# must still boot so the user can SSH in and fix the config or replace the drive.
#
# Dependencies: braid-module-single-disk (single-disk happy path),
# braid-module-bad-config (nofail boot-continue works).
{ braid }:
{ pkgs, ... }:
let
  passphrase = "testpassphrase";
  diskNames = [ "disk1" ];
in
{
  name = "braid-module-single-disk-dead";

  nodes.machine =
    { pkgs, ... }:
    {
      imports = [
        ../../modules/braid
        (import ./lib/initrd-fixture.nix {
          inherit passphrase diskNames;
          description = "Prepare LUKS + btrfs fixture then brick disk";
          postScript = ''
            # Brick the disk — zero the LUKS header so cryptsetup fails
            dd if=/dev/zero of=/dev/disk/by-id/virtio-disk1 bs=1M count=10
          '';
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
      ];
      virtualisation.memorySize = 2048;

    };

  testScript = builtins.readFile ./single-disk-dead.py;
}
