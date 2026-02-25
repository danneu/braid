# Test: braid-remove-disk-busy
#
# What: Verifies the best-effort `luksClose` behavior when the LUKS mapper is
# held open by another process: braid remove exits 0, logs a warning, and the
# mapper stays open.
#
# Why: pool.rs treats `cryptsetup close` as best-effort (warns to stderr, exits
# 0). The happy path is covered by braid-remove-disk, but no test confirms the
# warning IS surfaced and the mapper remains open when a process holds an fd.
#
# Scenario: Admin removes a pool member while a background process has an open
# fd on the mapper device. braid remove must still succeed (disk is out of
# btrfs) but the kernel refuses cryptsetup close.
{ braid }:
{
  name = "braid-remove-disk-busy";

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
      pkgs.coreutils
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

  testScript = builtins.readFile ./braid-remove-disk-busy.py;
}
