# Test: braid-module-bad-config
#
# What: Enables the braid module with disk paths that don't exist. No virtual
# disks are attached. The mount fails (no mapper devices). Boot completes
# anyway thanks to nofail on the mount.
#
# Why: Validates the "all drives dead / wrong config" tier of graceful failure.
# The OS lives on an internal SSD — a misconfigured or missing data pool must
# never prevent the system from booting. This is the critical gate test: if
# nofail doesn't let boot continue when the mount truly fails, we need a
# different approach.
#
# Dependencies: braid-module-disabled (module loads without error).
{ braid }:
{ lib, pkgs, ... }:
{
  name = "braid-module-bad-config";

  nodes.machine = { pkgs, ... }: {
    imports = [ ../../modules/braid ];

    braid = {
      enable = true;
      package = braid;
      disks = {
        phantom1 = { byId = "/dev/disk/by-id/phantom1"; };
        phantom2 = { byId = "/dev/disk/by-id/phantom2"; };
      };
    };

    # No virtualisation.emptyDiskImages — the block devices never appear.
    virtualisation.memorySize = 2048;

    # Re-declare mount for VM compat (qemu-vm.nix clobbers fileSystems).
    # Mirrors the module's mount settings exactly.
    virtualisation.fileSystems."/mnt/storage" = {
      device = "/dev/mapper/braid-phantom1";
      fsType = "btrfs";
      options = [
        "degraded"
        "nofail"
        "noatime"
        "skip_balance"
        "subvolid=5"
        "x-systemd.device-timeout=1s"
        "x-systemd.requires=btrfs-device-scan.service"
        "x-systemd.after=btrfs-device-scan.service"
      ];
    };
  };

  testScript = builtins.readFile ./bad-config.py;
}
