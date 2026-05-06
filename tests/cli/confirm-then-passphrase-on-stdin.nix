# Test: confirm-then-passphrase-on-stdin
#
# What: `braid add` and `braid replace` can consume confirmation and
# passphrase lines from the same piped stdin stream.
#
# Why: A buffered confirmation reader can pre-drain the passphrase line from
# fd 0 before the passphrase reader runs, breaking the command and retaining
# plaintext in an unzeroized std buffer.
#
# Dependencies: braid add and live replace paths.
{ braid }:
{
  name = "confirm-then-passphrase-on-stdin";

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

  testScript = builtins.readFile ./confirm-then-passphrase-on-stdin.py;
}
