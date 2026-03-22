# Test: braid-browse
#
# What: Boots a 2-disk RAID1 fixture pool, unlocks it, creates a subvolume,
# then runs `braid browse --check` to verify the non-interactive command
# pipeline: filesystem usage, subvolume list + parse, and subvolume drill-in.
#
# Why: Validates that the browse TUI's CmdRequest pipeline produces parseable
# output on a real btrfs pool — catches btrfs-progs output format changes that
# would break the TUI at runtime.
#
# Dependencies: braid-module-raid1 (RAID1 fixture setup).
{ braid }:
{ lib, pkgs, ... }:
let
  passphrase = "testpassphrase";
  diskNames = [
    "disk1"
    "disk2"
  ];
in
{
  name = "braid-browse";

  nodes.machine =
    { pkgs, ... }:
    {
      imports = [
        ../../modules/braid
        (import ../module/lib/initrd-fixture.nix {
          inherit passphrase diskNames;
          description = "Prepare LUKS + btrfs RAID1 fixture for browse test";
        })
      ];

      braid = {
        enable = true;
        package = braid;
        disks = lib.genAttrs diskNames (d: {
          byId = "/dev/disk/by-id/virtio-${d}";
        });
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

      virtualisation.fileSystems."/mnt/storage" = {
        device = "/dev/mapper/braid-disk1";
        fsType = "btrfs";
        options = [
          "degraded"
          "nofail"
          "noatime"
          "skip_balance"
          "x-systemd.device-timeout=1s"
          "x-systemd.requires=btrfs-device-scan.service"
          "x-systemd.after=btrfs-device-scan.service"
        ];
      };
    };

  testScript = builtins.readFile ./braid-browse.py;
}
