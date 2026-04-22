# Test: braid-module-bad-config
#
# What: Enables the braid module with no pool configured. No virtual disks are
# attached. The pool is offline at boot (the default state — there is no mount
# unit). Boot completes normally.
#
# Why: Validates that the module is inert when no pool exists. The OS lives on
# an internal SSD — a missing data pool must never prevent the system from
# booting.
#
# Dependencies: braid-module-disabled (module loads without error).
{ braid }:
{ pkgs, ... }:
{
  name = "braid-module-bad-config";

  nodes.machine =
    { pkgs, ... }:
    {
      imports = [ ../../modules/braid ];

      braid = {
        enable = true;
        package = braid;
      };

      # No virtualisation.emptyDiskImages — the block devices never appear.
      virtualisation.memorySize = 2048;
    };

  testScript = builtins.readFile ./bad-config.py;
}
