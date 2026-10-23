# Behavior lock for the mixed-profile convert replay; see the .py preamble.
{
  name = "repro-btrfs-convert-replay-enospc";

  nodes.machine =
    { pkgs, ... }:
    {
      virtualisation.emptyDiskImages = [
        {
          size = 6144;
          driveConfig.deviceExtraOpts.serial = "disk1";
        }
        {
          size = 6144;
          driveConfig.deviceExtraOpts.serial = "disk2";
        }
      ];
      virtualisation.memorySize = 2048;
      environment.systemPackages = [
        pkgs.btrfs-progs
        pkgs.lvm2
      ];
    };

  testScript =
    "import json\n\n"
    + builtins.readFile ../module/dm_delay_helpers.py
    + "\n\n"
    + builtins.readFile ./btrfs-convert-replay-enospc.py;
}
