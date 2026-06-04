# Test: braid-lock-probe-failed
#
# What: Verifies that `braid lock` falls back to UUID-scanned mapper cleanup
# when a mounted braid-owned pool contains a non-mapper btrfs device path that
# makes per-device probing fail.
#
# Why: The mounted `Snapshot::ProbeFailed` arm has behavior no mock can prove:
# real `btrfs filesystem show` output can make `probe_pool` fail while
# `probe_fsid` still succeeds, and lock must not abort in that state.
#
# Scenario: An operator manually adds a raw spare disk to braid's mounted pool
# with `btrfs device add`, bypassing braid. `braid lock` must unmount the pool
# and close only UUID-verified braid member mappers while skipping an
# unverified braid-prefixed candidate.
{ braid }:
{
  name = "braid-lock-probe-failed";

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
          driveConfig.deviceExtraOpts.serial = "spare";
        }
      ];

      environment.systemPackages = [
        braid
        pkgs.btrfs-progs
        pkgs.cryptsetup
      ];

      environment.etc."braid/config.json".text = builtins.toJSON {
        mount_point = "/mnt/storage";
      };
    };

  testScript = builtins.readFile ./braid-lock-probe-failed.py;
}
