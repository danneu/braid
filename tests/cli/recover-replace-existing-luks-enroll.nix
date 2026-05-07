# Test: recover-replace-existing-luks-enroll
#
# What: `braid recover` after a crash mid-`replace --enroll DIR`
# against a `PresentLuks` new disk replays the enrollment + header
# backup, then rolls back to pre-replace topology and clears the
# journal. The replayed enrollment makes auto-unlock work for the
# disk after recovery (slot 1 populated).
#
# Why: pre-refactor, the ExistingLuks recovery arm just rolled back
# without replaying any LUKS mutation. Adding `enroll_key_file:
# Some(kf)` to that variant means recovery now runs `cryptsetup
# luksAddKey` + header backup before clearing the journal -- this
# test pins that the journaled mutation actually replays.
{ braid }:
{
  name = "recover-replace-existing-luks-enroll";

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
      ];

      environment.etc."braid/config.json".text = builtins.toJSON {
        mount_point = "/mnt/storage";
      };
    };

  testScript = builtins.readFile ./recover-replace-existing-luks-enroll.py;
}
