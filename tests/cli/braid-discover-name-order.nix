# Test: braid-discover-name-order
#
# What: Boots a 2-disk LUKS fixture whose UUID order is opposite name order,
# then checks read-only discover output.
#
# Why: `braid discover` is operator-facing; decision 024 requires DiskName
# ordering even though pool membership is UUID-keyed on disk.
#
# Dependencies: initrd-fixture with explicit LUKS UUID assignments.
{ braid }:
{ pkgs, ... }:
let
  passphrase = "testpassphrase";
  diskNames = [
    "alpha"
    "zeta"
  ];
  diskUuidMap = {
    zeta = "11111111-1111-1111-1111-111111111111";
    alpha = "99999999-9999-9999-9999-999999999999";
  };
in
{
  name = "braid-discover-name-order";

  nodes.machine =
    { pkgs, ... }:
    {
      imports = [
        ../../modules/braid
        (import ../module/lib/initrd-fixture.nix {
          inherit passphrase diskNames diskUuidMap;
          description = "Prepare inverse UUID/name LUKS fixture for discover ordering";
        })
      ];

      braid = {
        enable = true;
        package = braid;
      };

      virtualisation.emptyDiskImages = [
        {
          size = 256;
          driveConfig.deviceExtraOpts.serial = "alpha";
        }
        {
          size = 256;
          driveConfig.deviceExtraOpts.serial = "zeta";
        }
      ];
      virtualisation.memorySize = 2048;

      environment.systemPackages = [ pkgs.btrfs-progs ];
    };

  testScript = builtins.readFile ./braid-discover-name-order.py;
}
