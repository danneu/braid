# Test: braid-remove-disk-busy
#
# What: Verifies the best-effort `luksClose` behavior when the LUKS mapper is
# held open by another process: braid remove exits 0, logs a warning, and the
# mapper stays open.
#
# Why: pool.rs treats `cryptsetup close` as best-effort (warns to stderr, exits
# 0). The happy path is covered by braid-remove-disk, but no test confirms the
# warning IS surfaced and the mapper remains open when a process holds an fd.
#
# Scenario: Admin removes a pool member while a loop device is attached to the
# mapper, holding it busy. braid remove must still succeed (disk is out of
# btrfs) but the kernel refuses cryptsetup close.
{ braid }:
{
  name = "braid-remove-disk-busy";

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
        pkgs.coreutils
      ];

      environment.etc."braid/config.json".text = builtins.toJSON {
        mount_point = "/mnt/storage";
      };
    };

  testScript = builtins.readFile ./braid-remove-disk-busy.py;
}
