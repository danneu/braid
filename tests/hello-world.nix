# Test: hello-world
#
# What: Boots a NixOS VM with three virtual drives and verifies they appear
# as block devices at the expected /dev/disk/by-id paths.
#
# Why: This is the foundation test. Everything else (LUKS, btrfs, samba)
# depends on the VM test infrastructure working and virtual drives being
# addressable by serial number. If this fails, nothing above it can work.
#
# Dependencies: None — this is the first test.
{
  name = "hello-world";

  nodes.machine =
    { ... }:
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
    };

  testScript = builtins.readFile ./hello-world.py;
}
