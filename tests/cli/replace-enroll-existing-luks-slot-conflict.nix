# Test: replace-enroll-existing-luks-slot-conflict
#
# What: `braid replace --enroll DIR` against a new disk whose slot 1
# is already occupied by an unknown key must refuse cleanly with the
# canonical luksKillSlot remediation text -- before any journal
# write, before any LUKS mutation.
#
# Why: This is the slot-1 conflict guard inside `plan_single_disk_
# enrollment`. A regression that proceeds anyway (overwriting an
# unknown keyslot, or stranding a journal that recovery can't replay)
# would surface here.
{ braid }:
{
  name = "replace-enroll-existing-luks-slot-conflict";

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

  testScript = builtins.readFile ./replace-enroll-existing-luks-slot-conflict.py;
}
