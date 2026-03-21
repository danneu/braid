# Repro: udev events on quiet missing-disk event
#
# Proves what raw block-layer udev monitoring reports when a mounted btrfs
# RAID1 member disappears underneath a LUKS+btrfs stack and no follow-up
# filesystem I/O is forced.
#
# Uses 3 disks (512 MiB each), no braid dependency.
{
  name = "repro-udev-missing-disk-idle";

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
      pkgs.coreutils
      pkgs.cryptsetup
      pkgs.kmod
      pkgs.util-linux
    ];
  };

  testScript = builtins.readFile ./udev-missing-disk-idle.py;
}
