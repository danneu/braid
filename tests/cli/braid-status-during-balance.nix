# Test: braid status during balance
#
# What: Validates that braid status succeeds while a RAID1 balance is in
# progress, when the data ratio is a fractional value like "1.01".
#
# Why: During a single→RAID1 balance, btrfs reports intermediate data ratios.
# The parser previously only accepted "1.00" and "2.00", causing braid status
# to hard-error during any balance operation.
#
# Dependencies: Rust braid binary for all commands.
{ braid }:
{
  name = "braid-status-during-balance";

  nodes.machine =
    { pkgs, ... }:
    {
      virtualisation.emptyDiskImages = [
        {
          size = 4096;
          driveConfig.deviceExtraOpts.serial = "disk1";
        }
        {
          size = 4096;
          driveConfig.deviceExtraOpts.serial = "disk2";
        }
      ];

      virtualisation.memorySize = 2048;

      environment.systemPackages = [
        braid
        pkgs.cryptsetup
        pkgs.btrfs-progs
        pkgs.jq
      ];

      environment.etc."braid/config.json".text = builtins.toJSON {
        disks = {
          disk1 = {
            by_id = "/dev/disk/by-id/virtio-disk1";
          };
          disk2 = {
            by_id = "/dev/disk/by-id/virtio-disk2";
          };
        };
        mount_point = "/mnt/storage";
      };
    };

  testScript = builtins.readFile ./braid-status-during-balance.py;
}
