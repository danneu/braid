# Repro: 2->1 remove with an undersized survivor.
#
# Two disks with a clear size gap (4 GiB + 1 GiB). RAID1 capacity pre-remove
# is bounded by the smaller device (1 GiB), but the logical used bytes can
# still exceed what the smaller device alone can hold once the eviction
# balances data to `single` and metadata/system to `DUP` on the survivor --
# the RAID1 profile only required one copy per chunk across two devices,
# while DUP requires two copies on one device.
#
# Without the new preflight, `braid remove disk1` skips `check_eviction_space`
# at `remaining == 1`, writes pending-op.json, and falls into the irreversible
# `btrfs device remove` path that either ENOSPCs mid-migration or crashes
# the filesystem read-only.
#
# See docs/design/decisions/012-intent-cli.md ("ENOSPC pre-flight check") for the
# updated invariant.
{ braid }:
{
  name = "remove-2to1-undersized-survivor";

  nodes.machine =
    { pkgs, ... }:
    {
      virtualisation.emptyDiskImages = [
        {
          size = 8192;
          driveConfig.deviceExtraOpts.serial = "disk1";
        }
        {
          size = 2048;
          driveConfig.deviceExtraOpts.serial = "disk2";
        }
      ];

      environment.systemPackages = [
        braid
        pkgs.cryptsetup
        pkgs.btrfs-progs
        # python3 is used in the testScript to create many small inline
        # files quickly (one exec, a tight loop) -- much faster than a
        # shell for-loop, which matters because we need ~400k files to
        # drive Metadata.used past the 2->1 capacity threshold.
        pkgs.python3
      ];

      environment.etc."braid/config.json".text = builtins.toJSON {
        mount_point = "/mnt/storage";
      };
    };

  testScript = builtins.readFile ./remove-2to1-undersized-survivor.py;
}
