# Test: LUKS header auto-backup on init-disk, corrupt header restore + data recovery
#
# What: Verifies that braid init-disk automatically backs up LUKS headers,
# and that a corrupted header can be restored from backup to recover data.
#
# Why: LUKS header corruption means permanent data loss regardless of knowing
# the passphrase. init-disk is the only luksFormat path (Principle 3), so
# auto-backup here guarantees every formatted disk has a recoverable header.
#
# Dependencies: LUKS primitives, btrfs basics, Rust braid binary with init-disk.
{ braid }:
{
  name = "luks-header-backup";

  nodes.machine = { pkgs, ... }: {
    virtualisation.emptyDiskImages = [
      { size = 256; driveConfig.deviceExtraOpts.serial = "disk1"; }
      { size = 256; driveConfig.deviceExtraOpts.serial = "disk2"; }
    ];

    environment.systemPackages = [
      braid
      pkgs.cryptsetup
      pkgs.btrfs-progs
      pkgs.jq
    ];
  };

  testScript = builtins.readFile ./luks-header-backup.py;
}
