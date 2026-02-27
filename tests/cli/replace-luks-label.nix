# Test: replace-luks-label
#
# What: Verifies that `braid replace` sets the LUKS2 label to "braid-<name>"
# on the new disk it formats, just like `braid add` does.
#
# Why: LUKS labels make volumes identifiable during manual recovery or
# debugging (e.g. luksDump). `braid replace` was missing the --label flag
# when formatting, so replaced disks had no label — breaking the operator's
# ability to identify drives by braid name.
#
# Scenario: Operator replaces disk2 with disk3, then runs luksDump on the
# new drive and expects to see "braid-disk3" so they can match it back to
# the braid config.
{ braid }:
{
  name = "replace-luks-label";

  nodes.machine = { pkgs, ... }: {
    virtualisation.emptyDiskImages = [
      { size = 1024; driveConfig.deviceExtraOpts.serial = "disk1"; }
      { size = 1024; driveConfig.deviceExtraOpts.serial = "disk2"; }
      { size = 1024; driveConfig.deviceExtraOpts.serial = "disk3"; }
    ];

    environment.systemPackages = [
      braid
      pkgs.cryptsetup
      pkgs.btrfs-progs
    ];

    environment.etc."braid/config.json".text = builtins.toJSON {
      disks = {
        disk1 = { by_id = "/dev/disk/by-id/virtio-disk1"; };
        disk2 = { by_id = "/dev/disk/by-id/virtio-disk2"; };
        disk3 = { by_id = "/dev/disk/by-id/virtio-disk3"; };
      };
      mount_point = "/mnt/storage";
    };
  };

  testScript = builtins.readFile ./replace-luks-label.py;
}
