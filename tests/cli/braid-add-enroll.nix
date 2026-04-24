# Test: braid-add-enroll
#
# What: Verifies that `braid add --enroll` enrolls the keyfile
# into the new disk as part of the add operation.
#
# Why: The --enroll flag on add wires enrollment into the format
# path, reusing the passphrase already in scope. If the passphrase handoff
# from luks_format() to enroll_key_file() is wrong, enrollment silently
# fails.
{ braid }:
{
  name = "braid-add-enroll";

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
        # disk3 is the keyfile-asymmetry fixture: the pool (disk1+disk2)
        # carries keyslot-1 after Phase 2; adding disk3 without --enroll
        # must surface the `[warn]` / `WARNING:` keyfile-asymmetry
        # diagnostic on the expected stream.
        {
          size = 1024;
          driveConfig.deviceExtraOpts.serial = "disk3";
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

  testScript = builtins.readFile ./braid-add-enroll.py;
}
