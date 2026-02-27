# Repro: btrfs replace start preserves the replaced device's devid
#
# Proves that `btrfs replace start` keeps the original devid on the new device,
# unlike `btrfs device add` + `btrfs device remove` which assigns a new devid.
# Also confirms that `btrfs filesystem resize <devid>:max` works after replace
# to use a larger replacement disk's full capacity.
#
# Uses 3 disks: two 512 MiB (initial RAID1) and one 1024 MiB (replacement).
{
  name = "repro-btrfs-replace-preserves-devid";

  nodes.machine = { pkgs, ... }: {
    virtualisation.emptyDiskImages = [
      { size = 512; driveConfig.deviceExtraOpts.serial = "disk1"; }
      { size = 512; driveConfig.deviceExtraOpts.serial = "disk2"; }
      { size = 1024; driveConfig.deviceExtraOpts.serial = "disk3"; }
    ];

    environment.systemPackages = [
      pkgs.cryptsetup
      pkgs.btrfs-progs
    ];
  };

  testScript = builtins.readFile ./btrfs-replace-preserves-devid.py;
}
