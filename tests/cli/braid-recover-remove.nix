# Test: braid recover (remove)
#
# What: Verifies `braid recover` correctly rebuilds pool.json after a crash
# interrupts a `braid remove` operation, for both crash timing scenarios.
#
# Why: The existing braid-recover test only covers OpKind::Add. Remove recovery
# has a unique subtlety: the removed disk's LUKS container still exists, so
# union_memberships opens it during recovery, but probe_pool must correctly
# exclude it from the rebuilt membership.
{ braid }:
{
  name = "braid-recover-remove";

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

  testScript = builtins.readFile ./braid-recover-remove.py;
}
