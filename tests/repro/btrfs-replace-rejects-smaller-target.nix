# Repro: btrfs replace start rejects a smaller target device
#
# Confirms that `btrfs replace start` fails when the target device is smaller
# than the source, and captures the exact error message/code the kernel returns.
#
# Uses 3 disks: two 512 MiB (initial RAID1) and one 256 MiB (undersized target).
{
  name = "repro-btrfs-replace-rejects-smaller-target";

  nodes.machine = { pkgs, ... }: {
    virtualisation.emptyDiskImages = [
      { size = 512; driveConfig.deviceExtraOpts.serial = "disk1"; }
      { size = 512; driveConfig.deviceExtraOpts.serial = "disk2"; }
      { size = 256; driveConfig.deviceExtraOpts.serial = "disk3"; }
    ];

    environment.systemPackages = [
      pkgs.cryptsetup
      pkgs.btrfs-progs
    ];
  };

  testScript = builtins.readFile ./btrfs-replace-rejects-smaller-target.py;
}
