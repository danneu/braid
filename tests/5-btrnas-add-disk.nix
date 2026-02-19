# Test: btrnas-add-disk
#
# What: Runs the btrnas-add-disk script through its full lifecycle: first disk
# (creates pool), second disk (converts to RAID1), third disk (expands pool),
# plus validation errors, crash recovery, and unmounted pool guard.
#
# Why: This is the first real deliverable — the one imperative command that
# orchestrates LUKS + btrfs for new disks. Every primitive has been proven in
# isolation (luks, btrfs-raid1, grow, shrink, heal, degrade). This test proves
# the script ties them together correctly.
#
# Dependencies: btrfs-grow1 (single → RAID1 → 3-drive works manually).
{
  name = "btrnas-add-disk";

  nodes.machine = { pkgs, ... }: {
    virtualisation.emptyDiskImages = [
      { size = 256; driveConfig.deviceExtraOpts.serial = "disk1"; }
      { size = 256; driveConfig.deviceExtraOpts.serial = "disk2"; }
      { size = 256; driveConfig.deviceExtraOpts.serial = "disk3"; }
      { size = 256; driveConfig.deviceExtraOpts.serial = "disk4"; }
      { size = 256; driveConfig.deviceExtraOpts.serial = "disk5"; }
    ];

    environment.systemPackages = [
      (import ../nix/btrnas-add-disk.nix { inherit pkgs; })
      pkgs.cryptsetup
      pkgs.btrfs-progs
    ];
  };

  testScript = builtins.readFile ./btrnas-add-disk.py;
}
