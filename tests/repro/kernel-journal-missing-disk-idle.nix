# Repro: kernel journal on quiet missing-disk event
#
# Proves what the pinned NixOS/kernel stack logs when a mounted btrfs RAID1
# member disappears underneath a LUKS+btrfs stack and no follow-up filesystem
# I/O is forced.
#
# Uses 3 disks (512 MiB each), no braid dependency.
{
  name = "repro-kernel-journal-missing-disk-idle";

  nodes.machine = { pkgs, ... }: {
    virtualisation.emptyDiskImages = [
      {
        size = 512;
        driveConfig.deviceExtraOpts = {
          serial = "disk1";
          id = "disk1dev";
        };
      }
      {
        size = 512;
        driveConfig.deviceExtraOpts = {
          serial = "disk2";
          id = "disk2dev";
        };
      }
      {
        size = 512;
        driveConfig.deviceExtraOpts = {
          serial = "disk3";
          id = "disk3dev";
        };
      }
    ];

    environment.systemPackages = [
      pkgs.btrfs-progs
      pkgs.cryptsetup
      pkgs.kmod
      pkgs.util-linux
    ];
  };

  testScript = builtins.readFile ./kernel-journal-missing-disk-idle.py;
}
