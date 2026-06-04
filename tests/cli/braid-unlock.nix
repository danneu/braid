# Test: braid unlock
#
# What: Verifies `braid unlock` opens LUKS volumes and mounts the btrfs pool.
#
# Why: After a reboot or missed initrd unlock, users need a single command to
# open all LUKS volumes and mount the pool. This tests the happy path,
# idempotency, partial state recovery, degraded mode, wrong passphrase
# rejection, and uninitialized disk detection.
{ braid }:
{
  name = "braid-unlock";

  nodes.machine =
    { pkgs, ... }:
    {
      virtualisation.emptyDiskImages = [
        {
          size = 1024;
          driveConfig.deviceExtraOpts.serial = "disk1";
        }
        {
          size = 1024;
          driveConfig.deviceExtraOpts.serial = "disk2";
        }
        {
          size = 1024;
          driveConfig.deviceExtraOpts.serial = "disk3";
        }
        {
          size = 1024;
          driveConfig.deviceExtraOpts.serial = "disk4";
        }
      ];

      environment.systemPackages = [
        braid
        pkgs.cryptsetup
        pkgs.btrfs-progs
        pkgs.lvm2
      ];

      environment.etc."braid/config.json".text = builtins.toJSON {
        mount_point = "/mnt/storage";
      };
    };

  testScript =
    builtins.readFile ./../module/dm_delay_helpers.py
    + "\n\n"
    + builtins.readFile ./../module/balance_helpers.py
    + "\n\n"
    + builtins.readFile ./braid-unlock.py;
}
