# Test: pool-lock-discover-contention
#
# What: Verifies `braid discover --write` is serialized against the pool
# operation lock before it can scan devices or write pool.json, while bare
# read-only discover does not take the lock.
#
# Why: `discover --write` rebuilds pool.json after a multi-device scan. Without
# Rust-level serialization, concurrent state writers can both pass the
# missing-pool.json gate and the later writer can replace the first result.
{ braid }:
{ pkgs, lib, ... }:
{
  name = "pool-lock-discover-contention";

  nodes.machine =
    { pkgs, lib, ... }:
    {
      imports = [
        ../../modules/braid
        (import ./lib/initrd-fixture.nix {
          passphrase = "testpassphrase";
          diskNames = [ "disk1" ];
          description = "Prepare LUKS + btrfs fixture for discover lock-contention test";
        })
      ];

      braid = {
        enable = true;
        package = braid;
      };

      virtualisation.emptyDiskImages = [
        {
          size = 512;
          driveConfig.deviceExtraOpts.serial = "disk1";
        }
      ];
      virtualisation.memorySize = 1024;
    };

  testScript = builtins.readFile ./pool-lock-discover-contention.py;
}
