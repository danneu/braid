# Test: braid idle exit codes
#
# What: Validates that `braid idle` returns exit 0 when the pool is idle or
# offline, and exit 1 when a btrfs operation is running.
#
# Why: braid idle is the integration point for autosuspend — incorrect exit
# codes would either prevent the NAS from ever sleeping (false busy) or allow
# sleep during active I/O operations (false idle).
#
# Scenario: 2-disk RAID1 pool. Check idle when pool is offline, then unlock
# and check idle when pool is idle. Start a scrub and check for busy.
{ braid }:
{
  name = "braid-idle";

  nodes.machine = { pkgs, ... }: {
    virtualisation.emptyDiskImages = [
      { size = 512; driveConfig.deviceExtraOpts.serial = "disk1"; }
      { size = 512; driveConfig.deviceExtraOpts.serial = "disk2"; }
    ];

    environment.systemPackages = [
      braid
      pkgs.cryptsetup
      pkgs.btrfs-progs
    ];

    environment.etc."braid/config.json".text = builtins.toJSON {
      disks = {
        disk1 = { by_id = "/dev/disk/by-id/virtio-disk1"; };
        disk2 = { by_id = "/dev/disk/by-id/virtio-disk2"; };
      };
      mount_point = "/mnt/storage";
    };
  };

  testScript = builtins.readFile ./braid-idle.py;
}
