# Repro: btrfs device remove missing — partial relocation crash
#
# Reproduces btrfs failure mode 2: when surviving devices have SOME
# unallocated space but not enough, `btrfs device remove missing` starts
# relocating, partially succeeds, then hits ENOSPC mid-transaction.
# The transaction abort forces the filesystem read-only.
#
# On real hardware with slow USB drives, this hangs for hours before
# crashing. In a VM with fast virtual disks, the crash happens in ~40s.
#
# See docs/claude-enospc-vs-hang.md for full analysis.
{ braid }:
{
  name = "btrfs-remove-enospc-crash";

  nodes.machine = { pkgs, ... }: {
    virtualisation.emptyDiskImages = [
      { size = 4096; driveConfig.deviceExtraOpts.serial = "disk1"; }
      { size = 4096; driveConfig.deviceExtraOpts.serial = "disk2"; }
      { size = 4096; driveConfig.deviceExtraOpts.serial = "disk3"; }
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

  testScript = builtins.readFile ./btrfs-remove-enospc-crash.py;
}
