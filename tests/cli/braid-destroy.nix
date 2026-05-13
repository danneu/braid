# Test: scripts/braid-destroy.sh
#
# What: End-to-end regression for the dev-only braid-destroy script: happy
# path tears down a live 2-disk pool; malformed or missing pool.json aborts
# before any env-side change.
#
# Why: braid-destroy.sh silently no-op'd the wipefs loop after commit
# 74feca5 moved pool membership from /etc/braid/config.json to
# /var/lib/braid/pool.json. The script nuked local state while leaving LUKS
# headers intact on every disk. This test pins the fix to pool.json as the
# source of truth, the UUID-keyed schema sniff, the non-empty name/by_id
# validator, and the "reject before braid lock" ordering that keeps a
# malformed membership from unmounting the live pool.
#
# Dependencies: braid add (builds the test pool).
{ braid }:
{
  name = "braid-destroy";

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
        pkgs.jq
      ];

      environment.etc."braid/config.json".text = builtins.toJSON {
        mount_point = "/mnt/storage";
      };

      # Install the real repo script so the test exercises the literal file.
      environment.etc."braid-destroy.sh" = {
        source = ../../scripts/braid-destroy.sh;
        mode = "0755";
      };
    };

  testScript = builtins.readFile ./braid-destroy.py;
}
