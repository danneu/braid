# Repro: btrfs replace interrupted mid-flight by VM crash
#
# Investigates what happens to a btrfs RAID1 pool when a `btrfs replace`
# operation is interrupted mid-flight by an unclean VM crash on the pinned
# NixOS toolchain. Per reference/btrfs-progs/Documentation/btrfs-replace.rst,
# v6.19+ kernels cancel an interrupted replace and require the user to restart
# from scratch — this test pins down whether that cancellation surfaces
# cleanly through `btrfs replace status`, `braid status`, and `braid recover`.
#
# 4 disks: three pool members (disk1/2/3) and one replacement target (disk4).
# Each is 1024 MiB so the replace has measurable work without inflating runtime.
{ braid }:
{
  name = "btrfs-replace-interrupted-mid-flight";

  nodes.machine = { pkgs, ... }: {
    virtualisation.emptyDiskImages = [
      { size = 1024; driveConfig.deviceExtraOpts.serial = "disk1"; }
      { size = 1024; driveConfig.deviceExtraOpts.serial = "disk2"; }
      { size = 1024; driveConfig.deviceExtraOpts.serial = "disk3"; }
      { size = 1024; driveConfig.deviceExtraOpts.serial = "disk4"; }
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

  testScript = builtins.readFile ./btrfs-replace-interrupted-mid-flight.py;
}
