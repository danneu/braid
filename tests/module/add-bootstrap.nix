# Test: braid-module-add-bootstrap
#
# What: Enables the braid module with a single raw disk (no initrd fixture).
# The test script runs `braid add` to bootstrap the pool from scratch, then
# verifies that Rust dispatch sets mount point permissions to root:storage 2770.
#
# Why: braid add mounts from the Rust CLI, not through a systemd service.
# The Rust-side `mark_online` permission fixup via `pool_access_group` must
# cover this path. Without this test, a regression in Rust dispatch would
# silently leave the mount root as root:root 0755, blocking non-root access.
#
# Dependencies: braid-module-single-disk (module loads correctly),
# hello-world (VM infra).
{ braid }:
{ pkgs, ... }:
{
  name = "braid-module-add-bootstrap";

  nodes.machine =
    { pkgs, ... }:
    {
      imports = [ ../../modules/braid ];

      braid = {
        enable = true;
        package = braid;
      };

      virtualisation.emptyDiskImages = [
        {
          size = 256;
          driveConfig.deviceExtraOpts.serial = "disk1";
        }
      ];
      virtualisation.memorySize = 2048;

      environment.systemPackages = [ pkgs.btrfs-progs ];

    };

  testScript = builtins.readFile ./add-bootstrap.py;
}
