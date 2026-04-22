# Repro: btrfs scan registry holds device references after umount
#
# After umount of a multi-device btrfs RAID1, the btrfs kernel scan registry
# still lists the devices (`btrfs fi show` shows them). `btrfs device scan
# --forget` clears the registry. Documents whether `cryptsetup close` succeeds
# or fails in each state (the race is timing-dependent).
#
# Uses 2 disks (512 MiB each), no braid dependency.
{
  name = "repro-cryptsetup-close-btrfs-held";

  nodes.machine =
    { pkgs, ... }:
    {
      virtualisation.emptyDiskImages = [
        {
          size = 512;
          driveConfig.deviceExtraOpts.serial = "disk1";
        }
        {
          size = 512;
          driveConfig.deviceExtraOpts.serial = "disk2";
        }
      ];

      environment.systemPackages = [
        pkgs.cryptsetup
        pkgs.btrfs-progs
      ];
    };

  testScript = builtins.readFile ./cryptsetup-close-btrfs-held.py;
}
