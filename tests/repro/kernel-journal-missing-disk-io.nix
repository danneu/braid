# Repro: kernel journal on missing-disk event followed by filesystem I/O
#
# Proves what the pinned NixOS/kernel stack logs when a mounted btrfs RAID1
# member disappears underneath a LUKS+btrfs stack and the filesystem later
# performs reads and writes.
#
# Uses 3 disks (512 MiB each), no braid dependency.
{
  name = "repro-kernel-journal-missing-disk-io";

  nodes.machine =
    { pkgs, ... }:
    {
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

  testScript = builtins.readFile ./kernel-journal-missing-disk-io.py;
}
