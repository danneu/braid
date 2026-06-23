# Test: braid-add-uuid-swap-rejected
#
# What: `braid add` refuses a returned closed LUKS disk when the live LUKS UUID
# changes after planning but before execution.
#
# Why: the ClosedPresentLuks path must re-probe the by-id target at the
# open boundary so a swapped disk cannot be opened or journaled under the
# planning-time UUID.
{ braid }:
{
  name = "braid-add-uuid-swap-rejected";

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

  testScript =
    builtins.readFile ./member_helpers.py
    + "\n\n"
    + builtins.readFile ./braid-add-uuid-swap-rejected.py;
}
