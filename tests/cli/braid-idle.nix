# Test: braid idle exit codes
#
# Intent: Validates that `braid idle` returns exit 0 when the pool is idle or
# offline, and exit 1 when a btrfs operation is genuinely running.
#
# Why: braid idle is the integration point for autosuspend -- incorrect exit
# codes would either prevent the NAS from ever sleeping (false busy) or allow
# sleep during active I/O operations (false idle).
#
# Scenario: 2-disk RAID1 pool. Check idle when pool is offline, then unlock
# and check idle when pool is idle. Hold a live scrub running with dm-delay
# read throttling and check for busy deterministically.
{ braid }:
{
  name = "braid-idle";

  nodes.machine =
    { pkgs, ... }:
    {
      virtualisation.emptyDiskImages = [
        {
          size = 512;
          driveConfig.deviceExtraOpts.serial = "disk1";
        }
        {
          size = 512;
          driveConfig.deviceExtraOpts.serial = "disk2";
        }
      ];

      environment.systemPackages = [
        braid
        pkgs.cryptsetup
        pkgs.btrfs-progs
        pkgs.lvm2
      ];

      environment.etc."braid/config.json".text = builtins.toJSON {
        mount_point = "/mnt/storage";
      };
    };

  testScript =
    let
      dmDelayHelpers = builtins.readFile ./../module/dm_delay_helpers.py;
      braidIdleTest = builtins.readFile ./braid-idle.py;
    in
    dmDelayHelpers + "\n\n" + braidIdleTest;
}
