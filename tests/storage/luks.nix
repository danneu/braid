# Test: luks
#
# What: LUKS-encrypts three virtual drives, opens them, verifies the mapper
# devices exist and are usable block devices.
#
# Why: The entire stack sits on LUKS. This test proves cryptsetup works in
# the VM before we layer btrfs on top. If LUKS fails, nothing above it matters.
#
# Dependencies: hello-world (VM boots, virtual drives are present).
{
  name = "luks";

  nodes.machine =
    { pkgs, ... }:
    {
      virtualisation.emptyDiskImages = [
        {
          size = 256;
          driveConfig.deviceExtraOpts.serial = "disk1";
        }
        {
          size = 256;
          driveConfig.deviceExtraOpts.serial = "disk2";
        }
        {
          size = 256;
          driveConfig.deviceExtraOpts.serial = "disk3";
        }
      ];

      environment.systemPackages = [ pkgs.cryptsetup ];
    };

  testScript = builtins.readFile ./luks.py;
}
