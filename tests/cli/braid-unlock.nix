# Test: braid unlock
#
# What: Verifies `braid unlock` opens LUKS volumes and mounts the btrfs pool.
#
# Why: After a reboot or missed initrd unlock, users need a single command to
# open all LUKS volumes and mount the pool. This tests the happy path,
# idempotency, partial state recovery, degraded mode, wrong passphrase
# rejection, and uninitialized disk detection.
{ braid }:
{
  name = "braid-unlock";

  nodes.machine = { pkgs, ... }: {
    virtualisation.emptyDiskImages = [
      { size = 1024; driveConfig.deviceExtraOpts.serial = "disk1"; }
      { size = 1024; driveConfig.deviceExtraOpts.serial = "disk2"; }
      { size = 1024; driveConfig.deviceExtraOpts.serial = "disk3"; }
      { size = 1024; driveConfig.deviceExtraOpts.serial = "disk4"; }
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
        disk3 = { by_id = "/dev/disk/by-id/virtio-disk3"; };
      };
      mount_point = "/mnt/storage";
    };
  };

  testScript = builtins.readFile ./braid-unlock.py;
}
