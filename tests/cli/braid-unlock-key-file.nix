# Test: braid-unlock-key-file
#
# What: Verifies `--key-file` flag opens LUKS with a binary keyfile and
# that a wrong keyfile is rejected.
#
# Why: The keyfile unlock code path is entirely different from passphrase
# (no PBKDF, different cryptsetup flags, run() vs run_with_stdin). Must
# verify independently.
{ braid }:
{
  name = "braid-unlock-key-file";

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

  testScript = builtins.readFile ./braid-unlock-key-file.py;
}
