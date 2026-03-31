# Test: remove-no-membership
#
# What: `braid remove` must fail hard (not warn) when pool.json is missing.
#
# Why: remove.rs treats MembershipError::NotFound as a warning and proceeds
# with btrfs device eviction. This lets btrfs state diverge from the
# authoritative membership — exactly the inconsistency the membership
# system is meant to prevent.
#
# Dependencies: braid add (builds the test pool).
{ braid }:
{
  name = "remove-no-membership";

  nodes.machine = { pkgs, ... }: {
    virtualisation.emptyDiskImages = [
      { size = 1024; driveConfig.deviceExtraOpts.serial = "disk1"; }
      { size = 1024; driveConfig.deviceExtraOpts.serial = "disk2"; }
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

  testScript = builtins.readFile ./remove-no-membership.py;
}
