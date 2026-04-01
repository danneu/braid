# Test: capture tool output fixtures for nixos-25.11
#
# What: Sets up LUKS + btrfs RAID1 in a VM and captures the exact output of
# every command that the Rust parsers consume. Writes each output to a file
# and copies them out via copy_from_vm.
#
# Why: Golden-file fixtures lock parser contracts to the pinned toolchain version.
# Run this once after pinning to a new NixOS release to regenerate fixtures.
#
# Dependencies: LUKS, btrfs, cryptsetup, findmnt must all work.
{
  name = "capture-tool-fixtures";

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
      ];

      environment.systemPackages = with pkgs; [
        cryptsetup
        btrfs-progs
        util-linux
        jq
        coreutils
      ];
    };

  testScript = builtins.readFile ./capture-tool-fixtures.py;
}
