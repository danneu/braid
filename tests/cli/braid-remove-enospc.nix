# Test: braid remove — ENOSPC pre-flight rejection (live disk)
#
# What: Verifies that `braid remove` rejects when other devices lack space
# to absorb data from the device being removed.
#
# Why: Same ENOSPC risk as remove-missing — btrfs device remove relocates
# data off the target device. If remaining devices can't absorb it, btrfs
# will either fail instantly or crash the filesystem to read-only.
#
# Dependencies: braid add (builds the test pool).
{ braid }:
{
  name = "braid-remove-enospc";

  nodes.machine = { pkgs, ... }: {
    virtualisation.emptyDiskImages = [
      { size = 512; driveConfig.deviceExtraOpts.serial = "disk1"; }
      { size = 512; driveConfig.deviceExtraOpts.serial = "disk2"; }
      { size = 512; driveConfig.deviceExtraOpts.serial = "disk3"; }
    ];

    environment.systemPackages = [
      braid
      pkgs.cryptsetup
      pkgs.btrfs-progs
    ];

    environment.etc."braid/config.json".text = builtins.toJSON {
      mount_point = "/mnt/storage";
    };
  };

  testScript = builtins.readFile ./braid-remove-enospc.py;
}
