# Test: add-passphrase-mismatch
#
# What: Wrong passphrase is rejected before any destructive action, and
# pool.json (authoritative membership) is not mutated by a failed add.
#
# Why: If save_membership runs before passphrase verification, a failed
# add leaves pool.json claiming a disk that was never LUKS-formatted or
# added to btrfs — causing unlock to target the wrong mapper set.
#
# Dependencies: braid add (builds the test pool).
{ braid }:
{
  name = "add-passphrase-mismatch";

  nodes.machine = { pkgs, ... }: {
    virtualisation.emptyDiskImages = [
      { size = 1024; driveConfig.deviceExtraOpts.serial = "disk1"; }
      { size = 1024; driveConfig.deviceExtraOpts.serial = "disk2"; }
      { size = 1024; driveConfig.deviceExtraOpts.serial = "disk3"; }
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

  testScript = builtins.readFile ./add-passphrase-mismatch.py;
}
