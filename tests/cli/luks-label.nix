# Test: luks-label
#
# What: Verifies that `braid add` sets the LUKS2 label to "braid-<name>" on
# each volume it formats.
#
# Why: LUKS labels make volumes identifiable during manual recovery or
# debugging (e.g. luksDump). If the label is missing or wrong, an operator
# loses the mapping between raw device and braid disk name.
#
# Scenario: Operator runs luksDump on an unknown drive and expects to see
# "braid-disk1" so they can match it back to the braid config.
{ braid }:
{
  name = "luks-label";

  nodes.machine = { pkgs, ... }: {
    virtualisation.emptyDiskImages = [
      { size = 256; driveConfig.deviceExtraOpts.serial = "disk1"; }
    ];

    environment.systemPackages = [
      braid
      pkgs.cryptsetup
      pkgs.btrfs-progs
    ];

    environment.etc."braid/config.json".text = builtins.toJSON {
      disks = {
        disk1 = { by_id = "/dev/disk/by-id/virtio-disk1"; };
      };
      mount_point = "/mnt/storage";
    };
  };

  testScript = builtins.readFile ./luks-label.py;
}
