# Test: btrfs-enospc
#
# NOTE: This actually just trips "Input/output error" which might be generic;
# I'm not sure we really know unless we were to look at dmesg for ENOSPC error.
#
# What: Fills a single-profile btrfs nearly to capacity, adds a second drive,
# then attempts a RAID1 balance conversion. Captures the exact error output
# btrfs produces when it hits ENOSPC during balance.
#
# Why: We need the real error string btrfs emits so we can detect ENOSPC in
# pool.rs. This is an exploratory test — raw LUKS + btrfs commands, no braid CLI.
#
# Scenario: User has a nearly-full single drive, adds a second, and tries to
# convert to RAID1. The balance fails because there isn't enough free space to
# create mirror copies of all existing data.
{
  name = "btrfs-enospc";

  nodes.machine = { pkgs, ... }: {
    virtualisation.emptyDiskImages = [
      { size = 256; driveConfig.deviceExtraOpts.serial = "disk1"; }
      { size = 256; driveConfig.deviceExtraOpts.serial = "disk2"; }
    ];

    environment.systemPackages = [
      pkgs.cryptsetup
      pkgs.btrfs-progs
    ];
  };

  testScript = builtins.readFile ./btrfs-enospc.py;
}
